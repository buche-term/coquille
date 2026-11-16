use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::io::Error;
use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, exit};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use coquille::channel::write_all;
use coquille::util::{DEFAULT_TERMINAL_SIZE, buche_terminal_size, cwd, find_executable, tty_size};

const ESC: u8 = 0x1b;
const STX: u8 = 0x02;
const SOH: u8 = 0x01;
const START_LEN: usize = 4;
const END_SEQ: [u8; 2] = [ESC, SOH];

const DOC: &str = r#"
stdembed: test harness for the +ssh fd-multiplexing protocol.

  stdembed --unwrap/-u COMMAND [ARGS...]   (≡ +ssh)
    Runs COMMAND as a subprocess (the "channel").
    • subprocess stdout  → demux frames → local fds; plain → local stdout
    • local fd3/fd5      → wrap frames  → subprocess stdin
    • local stdin        → subprocess stdin (passthrough)

  stdembed --wrap/-w COMMAND [ARGS...]     (opposite end)
    Runs COMMAND with fd3/fd5 as socketpairs (bidirectional).
    • own stdin          → demux frames → subprocess fds; plain → subprocess stdin
    • subprocess fd3/fd5 → wrap frames  → own stdout
    • subprocess stdout  → own stdout (passthrough)

  stdembed -u stdembed -w COMMAND  ≡  COMMAND
"#;

static LAST_SIG: AtomicI32 = AtomicI32::new(0);
static MASTER_FD: AtomicI32 = AtomicI32::new(-1);
/// buche's terminal size (see buche_terminal_size), read once (0: not
/// given): the SIGWINCH handler can't read the environment.
static ENV_COLS: AtomicU16 = AtomicU16::new(0);
static ENV_ROWS: AtomicU16 = AtomicU16::new(0);

/// The readiness handshake. The wrapping end (-w) announces itself with an
/// empty frame on fd 0 (READY_FD) as soon as it is set up, its terminal in
/// raw mode. Until the unwrapping end (-u) has seen a frame (that one, or
/// any other: an older -w sends no announcement, but its frames mean it runs
/// too), it leaves fd3/fd5 input unread: sent earlier, it would reach a
/// remote terminal still in cooked mode (a login shell starting, say), to be
/// echoed back as text and then flushed. After READY_TIMEOUT, it stops
/// waiting (a channel without -w at the other end).
///
/// Writing the empty fd-0 frame is a no-op for an -u that predates this.
const READY_FD: i32 = 0;
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
static PEER_READY: AtomicBool = AtomicBool::new(false);

fn ready_frame() -> Vec<u8> {
    frame(READY_FD, &[])
}

/// Where -u sends a frame from the channel: to its local fd (the ready
/// announcement goes nowhere), and either way, the other end is up.
fn unwrap_sink(n: i32, d: &[u8]) {
    PEER_READY.store(true, Ordering::SeqCst);
    if n != READY_FD {
        write_all(n as RawFd, d);
    }
}

extern "C" fn record_sig(sig: c_int) {
    LAST_SIG.store(sig, Ordering::SeqCst);
}

extern "C" fn winch_handler(sig: c_int) {
    LAST_SIG.store(sig, Ordering::SeqCst);
    let fd = MASTER_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let ws = winsize_now();
        unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws as *const libc::winsize) };
    }
}

unsafe fn set_sigaction(sig: c_int, h: extern "C" fn(c_int)) {
    // SAFETY: zero is a valid bit pattern for sigaction (all fields are ints,
    // flags, or a null function pointer), and sig/h are valid signals/handlers.
    unsafe {
        let mut sa: libc::sigaction = MaybeUninit::zeroed().assume_init();
        sa.sa_sigaction = h as libc::sighandler_t;
        sa.sa_flags = 0;
        libc::sigaction(sig, &sa, ptr::null_mut());
    }
}

fn die_errno(what: &str) -> ! {
    let e = Error::last_os_error();
    eprintln!("stdembed: {what}: {e}");
    exit(1)
}

fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (sign, rest) = match t.as_bytes()[0] {
        b'+' => (1i64, &t[1..]),
        b'-' => (-1i64, &t[1..]),
        _ => (1i64, t),
    };
    if rest.is_empty() {
        return None;
    }
    let b = rest.as_bytes();
    let mut val: i64 = 0;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'0'..=b'9' => {
                val = val.checked_mul(10)?.checked_add((c - b'0') as i64)?;
            }
            b'_' => {
                if i == 0 || i + 1 == b.len() || !b[i - 1].is_ascii_digit() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(sign * val)
}

/// The size the command's PTY gets: buche's, else our terminal's, else the
/// default, dimension by dimension (as cq's: see terminal_size).
/// Async-signal-safe (see ENV_COLS).
fn winsize_now() -> libc::winsize {
    let (tty_cols, tty_rows) = tty_size(0).unwrap_or((0, 0));
    let pick = |env: u16, tty: u16, default: u16| [env, tty].into_iter().find(|&n| n > 0).unwrap_or(default);
    libc::winsize {
        ws_col: pick(ENV_COLS.load(Ordering::SeqCst), tty_cols, DEFAULT_TERMINAL_SIZE.0),
        ws_row: pick(ENV_ROWS.load(Ordering::SeqCst), tty_rows, DEFAULT_TERMINAL_SIZE.1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn init_env_size_cache() {
    let (cols, rows) = buche_terminal_size();
    ENV_COLS.store(cols.unwrap_or(0), Ordering::SeqCst);
    ENV_ROWS.store(rows.unwrap_or(0), Ordering::SeqCst);
}

fn set_winsize(fd: RawFd) {
    let ws = winsize_now();
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws as *const libc::winsize) };
}

fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn want_pty() -> bool {
    is_tty(0) || env::var_os("BUCHE_PTY_ROWS").is_some() || env::var_os("BUCHE_PTY_COLS").is_some()
}

fn close_fd(fd: RawFd) {
    unsafe { libc::close(fd) };
}

fn fd_read(fd: RawFd, n: usize) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(n);
    loop {
        let r = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, n) };
        if r < 0 {
            if Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Vec::new();
        }
        unsafe { buf.set_len(r as usize) };
        return buf;
    }
}

/// Read `fd` until its end, 4096 bytes at a time, handing each chunk to
/// `chunk`.
fn pump(fd: RawFd, mut chunk: impl FnMut(&[u8])) {
    loop {
        let data = fd_read(fd, 4096);
        if data.is_empty() {
            return;
        }
        chunk(&data);
    }
}

struct LockedWriter {
    fd: RawFd,
    lock: Mutex<()>,
}

impl LockedWriter {
    fn new(fd: RawFd) -> Arc<Self> {
        Arc::new(LockedWriter {
            fd,
            lock: Mutex::new(()),
        })
    }

    fn write(&self, data: &[u8]) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        write_all(self.fd, data);
    }
}

fn start_seq(fd: i32) -> Vec<u8> {
    let s = format!("{fd:02}");
    let mut v = Vec::with_capacity(2 + s.len());
    v.push(ESC);
    v.push(STX);
    v.extend_from_slice(s.as_bytes());
    v
}

/// `data`, framed as fd `fd`'s.
fn frame(fd: i32, data: &[u8]) -> Vec<u8> {
    let mut frame = start_seq(fd);
    frame.extend_from_slice(data);
    frame.extend_from_slice(&END_SEQ);
    frame
}

fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn parse_frame_fd(b: &[u8]) -> Option<i32> {
    if b.iter().any(|&c| c >= 0x80) {
        return None;
    }
    let s = std::str::from_utf8(b).ok()?;
    Some(py_int(s)? as i32)
}

struct StreamParser {
    buf: Vec<u8>,
    frame_fd: Option<i32>,
    frame_buf: Vec<u8>,
}

impl StreamParser {
    fn new() -> Self {
        StreamParser {
            buf: Vec::new(),
            frame_fd: None,
            frame_buf: Vec::new(),
        }
    }

    fn feed<F: FnMut(&[u8]), G: FnMut(i32, &[u8])>(&mut self, data: &[u8], pt: &mut F, sink: &mut G) {
        if self.frame_fd.is_none() {
            self.scan_normal(data, pt, sink);
        } else {
            self.scan_frame(data, pt, sink);
        }
    }

    fn scan_normal<F: FnMut(&[u8]), G: FnMut(i32, &[u8])>(&mut self, data: &[u8], pt: &mut F, sink: &mut G) {
        self.buf.extend_from_slice(data);
        while !self.buf.is_empty() {
            let esc_pos = match find_byte(&self.buf, ESC) {
                None => {
                    let b = std::mem::take(&mut self.buf);
                    pt(&b);
                    return;
                }
                Some(p) => p,
            };
            if esc_pos > 0 {
                let b: Vec<u8> = self.buf.drain(..esc_pos).collect();
                pt(&b);
            }
            if self.buf.len() < START_LEN {
                return;
            }
            if self.buf[1] == STX {
                match parse_frame_fd(&self.buf[2..4]) {
                    Some(fd) => {
                        self.buf.drain(..START_LEN);
                        self.frame_fd = Some(fd);
                        self.frame_buf.clear();
                        let rest = std::mem::take(&mut self.buf);
                        self.scan_frame(&rest, pt, sink);
                        return;
                    }
                    None => {
                        pt(&[ESC]);
                        self.buf.remove(0);
                    }
                }
            } else {
                pt(&[ESC]);
                self.buf.remove(0);
            }
        }
    }

    fn scan_frame<F: FnMut(&[u8]), G: FnMut(i32, &[u8])>(&mut self, data: &[u8], pt: &mut F, sink: &mut G) {
        self.frame_buf.extend_from_slice(data);
        let pos = match find_seq(&self.frame_buf, &END_SEQ) {
            Some(p) => p,
            None => return,
        };
        let fd = self.frame_fd.take().unwrap();
        let payload: Vec<u8> = self.frame_buf.drain(..pos).collect();
        self.frame_buf.drain(..END_SEQ.len());
        let rest = std::mem::take(&mut self.frame_buf);
        sink(fd, &payload);
        if !rest.is_empty() {
            self.scan_normal(&rest, pt, sink);
        }
    }
}

fn make_pipe() -> (RawFd, RawFd) {
    let mut f = [0 as RawFd; 2];
    if unsafe { libc::pipe(f.as_mut_ptr()) } != 0 {
        die_errno("pipe");
    }
    (f[0], f[1])
}

fn make_socketpair() -> (RawFd, RawFd) {
    let mut f = [0 as RawFd; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, f.as_mut_ptr()) } != 0 {
        die_errno("socketpair");
    }
    (f[0], f[1])
}

fn open_pty() -> (RawFd, RawFd) {
    let mut m: c_int = 0;
    let mut s: c_int = 0;
    if unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            ptr::null_mut::<c_char>(),
            ptr::null_mut::<libc::termios>(),
            ptr::null_mut::<libc::winsize>(),
        )
    } != 0
    {
        die_errno("openpty");
    }
    (m, s)
}

fn fork() -> libc::pid_t {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        die_errno("fork");
    }
    pid
}

fn wait_child(pid: libc::pid_t, kill_on_int: bool) -> c_int {
    let mut status: c_int = 0;
    let mut forwarded = false;
    loop {
        LAST_SIG.store(0, Ordering::SeqCst);
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r >= 0 {
            return status;
        }
        if Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            if kill_on_int && !forwarded && LAST_SIG.load(Ordering::SeqCst) == libc::SIGINT {
                unsafe { libc::kill(pid, libc::SIGINT) };
                forwarded = true;
            }
            continue;
        }
        return 255 << 8;
    }
}

fn exit_with_status(status: c_int) -> ! {
    if libc::WIFEXITED(status) {
        exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        exit(-libc::WTERMSIG(status));
    }
    exit(status & 0xff);
}

fn grab_raw_stdin() -> Option<libc::termios> {
    if !is_tty(0) {
        return None;
    }
    let mut t: libc::termios = unsafe { MaybeUninit::zeroed().assume_init() };
    if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
        return None;
    }
    let old = t;
    t.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
    t.c_oflag &= !libc::OPOST;
    t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
    t.c_cc[libc::VMIN as usize] = 1;
    t.c_cc[libc::VTIME as usize] = 0;
    unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, &t) };
    Some(old)
}

fn restore_stdin(old: Option<libc::termios>) {
    if let Some(t) = old {
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &t) };
    }
}

fn pump_extra_fds(w: &Arc<LockedWriter>, done: &AtomicBool) {
    let mut watches: Vec<RawFd> = Vec::new();
    for fd in [3 as RawFd, 5] {
        let mut st: libc::stat = unsafe { MaybeUninit::zeroed().assume_init() };
        if unsafe { libc::fstat(fd, &mut st) } == 0 {
            watches.push(fd);
        }
    }
    // Leave fd3/fd5 input queued until the other end is up (see PEER_READY).
    let since = std::time::Instant::now();
    while !PEER_READY.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst) && since.elapsed() < READY_TIMEOUT {
        thread::sleep(std::time::Duration::from_millis(10));
    }
    while !watches.is_empty() && !done.load(Ordering::SeqCst) {
        let mut pfds: Vec<libc::pollfd> = watches
            .iter()
            .map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 500) };
        if r < 0 {
            if Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        let mut dead: Vec<RawFd> = Vec::new();
        for p in &pfds {
            if p.revents != 0 {
                let chunk = fd_read(p.fd, 4096);
                if chunk.is_empty() {
                    dead.push(p.fd);
                } else {
                    w.write(&frame(p.fd, &chunk));
                }
            }
        }
        watches.retain(|f| !dead.contains(f));
    }
}

/// Frame what `fd` says as fd `fdnum`'s, onto `w`, until its end.
fn pump_framed(fd: RawFd, fdnum: i32, w: &LockedWriter) {
    pump(fd, |chunk| w.write(&frame(fdnum, chunk)));
}

fn child_error(msg: &str) -> ! {
    let line = format!("stdembed: child setup error: {msg}\n");
    write_all(2, line.as_bytes());
    unsafe { libc::_exit(127) };
}

/// In the child, once forked: its fds as `map` says (destination: source),
/// all others closed, then `cmd` (looked up in its PATH) run with `env`.
fn child_setup_and_exec(cmd: &[OsString], env: &BTreeMap<OsString, OsString>, map: &BTreeMap<RawFd, RawFd>) -> ! {
    const HI: RawFd = 100;
    let dsts: Vec<RawFd> = map.keys().copied().collect();
    let srcs: Vec<RawFd> = dsts.iter().map(|d| *map.get(d).unwrap()).collect();
    for (i, src) in srcs.iter().enumerate() {
        if unsafe { libc::dup2(*src, HI + i as RawFd) } < 0 {
            child_error(&format!("dup2 {src}: {}", Error::last_os_error()));
        }
    }
    for (i, dst) in dsts.iter().enumerate() {
        if unsafe { libc::dup2(HI + i as RawFd, *dst) } < 0 {
            child_error(&format!("dup2 {dst}: {}", Error::last_os_error()));
        }
    }
    for i in 0..dsts.len() {
        unsafe { libc::close(HI + i as RawFd) };
    }
    let keep = dsts.iter().max().copied().unwrap_or(2).max(2);
    let maxfd: RawFd = {
        let mut lim: libc::rlimit = unsafe { MaybeUninit::zeroed().assume_init() };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
            lim.rlim_cur.min(65536) as RawFd
        } else {
            1024
        }
    };
    for fd in (keep + 1)..maxfd {
        unsafe { libc::close(fd) };
    }
    let err = Command::new(&cmd[0]).args(&cmd[1..]).env_clear().envs(env).exec();
    child_error(&format!("{}: {err}", cmd[0].to_string_lossy()))
}

/// The command, running: its terminal side is a PTY (`input` and `output`
/// both its master), or pipes to its stdin and from its stdout.
struct Child {
    pid: libc::pid_t,
    input: RawFd,
    output: RawFd,
    pty: bool,
}

/// Start `cmd` with `env`: on a PTY (its stdin, stdout and stderr) if `pty`,
/// else with pipes for its stdin and stdout; its other fds as `fds` says
/// (destination, source: the sources are closed here once it has them), and
/// our `own` fds closed in it.
fn spawn(
    cmd: &[OsString],
    env: &BTreeMap<OsString, OsString>,
    pty: bool,
    fds: &[(RawFd, RawFd)],
    own: &[RawFd],
) -> Child {
    let mut map: BTreeMap<RawFd, RawFd> = fds.iter().copied().collect();
    let (input, output, theirs) = if pty {
        init_env_size_cache();
        let (master, slave) = open_pty();
        set_winsize(master);
        map.extend([(0, slave), (1, slave), (2, slave)]);
        (master, master, vec![slave])
    } else {
        let (in_r, in_w) = make_pipe();
        let (out_r, out_w) = make_pipe();
        map.extend([(0, in_r), (1, out_w)]);
        (in_w, out_r, vec![in_r, out_w])
    };
    let pid = fork();
    if pid == 0 {
        if pty {
            close_fd(input);
            unsafe { libc::setsid() };
            // `as _` because ioctl's request is c_ulong on macOS but c_int on Linux.
            unsafe { libc::ioctl(theirs[0], libc::TIOCSCTTY as _, 0) };
        }
        for &fd in own {
            close_fd(fd);
        }
        child_setup_and_exec(cmd, env, &map);
    }
    for fd in theirs.into_iter().chain(fds.iter().map(|&(_, src)| src)) {
        close_fd(fd);
    }
    if pty {
        MASTER_FD.store(input, Ordering::SeqCst);
        unsafe { set_sigaction(libc::SIGWINCH, winch_handler) };
    }
    Child {
        pid,
        input,
        output,
        pty,
    }
}

/// Wait for the command to end (a SIGINT forwarded to it, on a PTY), put
/// things back, close `fds`, and exit as it did.
fn finish(child: Child, old: Option<libc::termios>, fds: &[RawFd]) -> ! {
    let status = wait_child(child.pid, child.pty);
    restore_stdin(old);
    if child.pty {
        close_fd(child.input);
    }
    for &fd in fds {
        close_fd(fd);
    }
    exit_with_status(status)
}

/// -u: run the channel command; demultiplex what it outputs, and multiplex
/// our fd3/fd5 into its input (see DOC).
fn run_unwrap(cmd: &[OsString]) -> ! {
    let pty = want_pty();
    let env: BTreeMap<OsString, OsString> = env::vars_os().collect();
    if !pty {
        let path = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        let prog = cmd[0].to_string_lossy();
        if !find_executable(&prog, &path, &cwd()).is_some_and(|exe| exe.exists()) {
            eprintln!("FileNotFoundError: [Errno 2] No such file or directory: '{prog}'");
            exit(1);
        }
    }
    let child = spawn(cmd, &env, pty, &[], &[]);
    let w_out = LockedWriter::new(1);
    let w_child = LockedWriter::new(child.input);
    let done = Arc::new(AtomicBool::new(false));
    let old = grab_raw_stdin();

    {
        let (w, done, fd) = (w_out, done.clone(), child.output);
        thread::spawn(move || {
            let mut parser = StreamParser::new();
            pump(fd, |chunk| {
                parser.feed(chunk, &mut |d| w.write(d), &mut |n, d| unwrap_sink(n, d))
            });
            done.store(true, Ordering::SeqCst);
        });
    }
    {
        let (w, fd) = (w_child.clone(), child.input);
        thread::spawn(move || {
            pump(0, |chunk| w.write(chunk));
            if !pty {
                close_fd(fd);
            }
        });
    }
    thread::spawn(move || pump_extra_fds(&w_child, &done));

    finish(child, old, &[])
}

/// -w: run the command with fd3/fd5 (see DOC); demultiplex our input into
/// its, and multiplex its fd4/fd5 into our output.
fn run_wrap(cmd: &[OsString]) -> ! {
    let pty = want_pty();
    let (pipe3_r, pipe3_w) = make_pipe();
    let (pipe4_r, pipe4_w) = make_pipe();
    let (local5, remote5) = make_socketpair();
    let mut env: BTreeMap<OsString, OsString> = env::vars_os().collect();
    env.insert(OsString::from("BUCHE_CONTROL_FD"), OsString::from("5"));
    let child = spawn(cmd, &env, pty, &[(3, pipe3_r), (4, pipe4_w), (5, remote5)], &[local5]);
    let w_out = LockedWriter::new(1);
    let w_in = LockedWriter::new(child.input);
    let old = grab_raw_stdin();
    w_out.write(&ready_frame());

    {
        let (w, fd) = (w_out.clone(), child.output);
        thread::spawn(move || pump(fd, |chunk| w.write(chunk)));
    }
    {
        let fd = child.input;
        thread::spawn(move || {
            let mut parser = StreamParser::new();
            pump(0, |chunk| {
                parser.feed(chunk, &mut |d| w_in.write(d), &mut |n, d| match n {
                    3 => write_all(pipe3_w, d),
                    5 => write_all(local5, d),
                    _ => {}
                })
            });
            if !pty {
                close_fd(fd);
            }
        });
    }
    {
        let w = w_out.clone();
        thread::spawn(move || pump_framed(pipe4_r, 4, &w));
    }
    thread::spawn(move || pump_framed(local5, 5, &w_out));

    finish(child, old, &[pipe3_w, pipe4_r, local5])
}

fn py_repr(s: &str) -> String {
    let has_sq = s.contains('\'');
    let has_dq = s.contains('"');
    let q = if has_sq && !has_dq { '"' } else { '\'' };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

fn print_size() -> ! {
    match (buche_terminal_size(), tty_size(0)) {
        ((Some(cols), Some(rows)), _) => println!("('buche', {rows}, {cols})"),
        (_, Some((cols, rows))) => println!("('winsz', {rows}, {cols})"),
        _ => println!("b'\\x18\\x00P\\x00\\x00\\x00\\x00\\x00'"),
    }
    exit(0);
}

fn main() -> ! {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    unsafe { set_sigaction(libc::SIGINT, record_sig) };
    let mut argv: Vec<OsString> = env::args_os().collect();
    if argv.len() > 1 {
        argv.remove(0);
    }
    if argv.is_empty() {
        eprint!("{DOC}\n");
        exit(1);
    }
    let flag = argv.remove(0);
    if flag == "--help" || flag == "-h" {
        print!("{DOC}\n");
        exit(0)
    } else if (flag == "--unwrap" || flag == "-u" || flag == "--wrap" || flag == "-w") && argv.is_empty() {
        eprintln!("IndexError: list index out of range");
        exit(1)
    } else if flag == "--unwrap" || flag == "-u" {
        run_unwrap(&argv)
    } else if flag == "--wrap" || flag == "-w" {
        run_wrap(&argv)
    } else if flag == "--size" {
        print_size()
    } else {
        eprintln!("stdembed: unknown flag {}", py_repr(&flag.to_string_lossy()));
        exit(1)
    }
}
