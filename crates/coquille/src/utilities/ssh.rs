//! `+ssh HOST` (built as `cq-ssh`, see resources/+ssh): open a cq session on
//! a remote host, through stdembed.
//!
//! Equivalent to `stdembed -u ssh -t HOST "bash -lc 'stdembed -w cq'"`, but
//! first checks that coquille's cq and stdembed exist on the remote. If they
//! do not, it offers to install this release of coquille (cq, stdembed, sel,
//! watch, +ssh/cq-ssh) into `~/.local/bin` on the remote, using the GitHub
//! release archive that matches the remote's OS and architecture. The archive
//! is downloaded locally and streamed over ssh, so the remote needs no
//! internet access.
//!
//! Under buche (cq's control channel on $BUCHE_CONTROL_FD), everything
//! `+ssh` has to say (the prompt, the progress, errors) goes to a GUI cell,
//! never to stdout/stderr, which belong to the remote session. Without a
//! channel, it falls back to the terminal.

use std::cell::{Cell, RefCell};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio, exit};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

const USAGE: &str = "usage: +ssh HOST

Open a cq session on HOST (through stdembed). If cq is missing on HOST,
offer to install coquille into ~/.local/bin there.";

// Remote scripts run as `bash -lc '...'` or `sh -c '...'` from the user's
// login shell, whatever it is (bash, zsh, fish, csh...). To keep the quoting
// portable, they contain no single quotes, no `!` and no newlines.

/// Prints `CQPROBE <os> <arch> <status>`, where status is:
///  - `local`: cq and stdembed are in ~/.local/bin, where we install them
///    (preferred, wherever ~/.local/bin is on the PATH, if at all);
///  - `path`: coquille's cq, and stdembed, are on the login PATH;
///  - `missing`: neither.
///
/// A `cq` on the PATH is only taken if it is coquille's: some other program
/// may go by that name. It is recognised by a string in its binary (see
/// `about` in main.rs), without running it.
const PROBE: &str = concat!(
    "d=\"$HOME/.local/bin\"; ",
    "if [ -x \"$d/cq\" ] && [ -x \"$d/stdembed\" ]; then s=local; ",
    "else c=$(command -v cq); e=$(command -v stdembed); s=missing; ",
    "if [ -n \"$c\" ] && [ -n \"$e\" ] && [ -f \"$c\" ] && grep -q \"A shell for the Buche terminal\" \"$c\"; ",
    "then s=path; fi; fi; ",
    "echo CQPROBE \"$(uname -s)\" \"$(uname -m)\" $s",
);

/// Reads a .tar.xz on stdin and moves the executables it contains (under the
/// archive's top directory) into ~/.local/bin.
const INSTALL: &str = concat!(
    "d=\"$HOME/.local/bin\"; t=\"$d/.cq-install-$$\"; ",
    "mkdir -p \"$t\" && tar -xJf - -C \"$t\" && ",
    "for f in \"$t\"/*/*; do if [ -f \"$f\" ] && [ -x \"$f\" ]; then mv -f \"$f\" \"$d/\" || exit 1; fi; done; ",
    "s=$?; rm -rf \"$t\"; exit $s",
);

const RUN_ON_PATH: &str = "stdembed -w cq";
const RUN_LOCAL: &str = "PATH=\"$HOME/.local/bin:$PATH\" stdembed -w cq";

/// Options that let the probe, the install and the session share a single
/// ssh connection, so that the user authenticates only once.
fn control_opts() -> [&'static str; 6] {
    [
        "-o",
        "ControlMaster=auto",
        // /tmp rather than $TMPDIR: socket paths are limited to ~100 bytes.
        "-o",
        "ControlPath=/tmp/cq-ssh-%C",
        "-o",
        "ControlPersist=120",
    ]
}

fn ssh(host: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(control_opts());
    cmd.arg(host);
    cmd
}

fn shell(kind: &str, script: &str) -> String {
    format!("{kind} -c '{script}'")
}

/// Runs a helper (ssh, curl) whose own output is ours to report: stdin
/// closed, stdout and stderr captured.
fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
}

/// What a finished helper wrote on stderr (for error reports).
fn stderr_of(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut text);
    }
    text.trim().to_owned()
}

enum Status {
    OnPath,
    Local,
    Missing,
}

fn probe(ui: &Ui, host: &str) -> (String, String, Status) {
    let out = quiet(ssh(host).arg(shell("bash -l", PROBE)))
        .output()
        .unwrap_or_else(|e| ui.fail("Could not run ssh", e.to_string()));
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if !out.status.success() {
        ui.fail(format!("Could not connect to {host}"), stderr);
    }
    // The login profile may print things of its own: look for our line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some(line) = stdout.lines().rev().find_map(|l| l.trim().strip_prefix("CQPROBE ")) else {
        ui.fail(format!("Unexpected answer from {host}"), format!("{stdout}{stderr}"));
    };
    let parts: Vec<&str> = line.split_whitespace().collect();
    let [os, arch, status] = parts[..] else {
        ui.fail(format!("Unexpected answer from {host}"), line.to_owned());
    };
    let status = match status {
        "path" => Status::OnPath,
        "local" => Status::Local,
        _ => Status::Missing,
    };
    (os.to_string(), arch.to_string(), status)
}

/// The release target triple for a `uname -s` / `uname -m` pair.
fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-musl"),
        ("Darwin", "arm64" | "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// Where `+ssh` speaks: a GUI cell in buche (through the control channel),
/// or the terminal when there is no channel.
enum Ui {
    Gui { fd: RawFd, open: Cell<bool> },
    Tty { last_label: RefCell<String>, last_update: Cell<Option<Instant>> },
}

/// The address of `+ssh`'s cell, where it prompts and shows progress.
const DIALOG: [&str; 2] = ["$proc", "install"];
const DIALOG_HTML: &str = include_str!("../../resources/ssh-install.html");

const THROTTLE: Duration = Duration::from_millis(100);

/// The control channel, if there is one.
fn control_fd() -> Option<RawFd> {
    let fd: RawFd = env::var("BUCHE_CONTROL_FD").ok()?.parse().ok()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(fd)
}

fn wait_fd(fd: RawFd, events: libc::c_short) {
    let mut pfd = libc::pollfd { fd, events, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, -1) };
}

/// Writes one line on a file descriptor (which may be non-blocking).
fn write_line(fd: RawFd, line: &str) {
    let mut data = line.as_bytes().to_vec();
    data.push(b'\n');
    let mut written = 0;
    while written < data.len() {
        let n = unsafe { libc::write(fd, data[written..].as_ptr().cast(), data.len() - written) };
        if n >= 0 {
            written += n as usize;
            continue;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::EINTR) => wait_fd(fd, libc::POLLOUT),
            _ => return,
        }
    }
}

/// Reads one line from a file descriptor, a byte at a time: whatever
/// follows it is left for stdembed, which takes the channel over afterwards.
fn read_line(fd: RawFd) -> Option<String> {
    let mut line = Vec::new();
    loop {
        let mut byte = 0u8;
        let n = unsafe { libc::read(fd, (&raw mut byte).cast(), 1) };
        match n {
            1 if byte == b'\n' => return Some(String::from_utf8_lossy(&line).into_owned()),
            1 => line.push(byte),
            0 => return None,
            _ => match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINTR) => wait_fd(fd, libc::POLLIN),
                _ => return None,
            },
        }
    }
}

fn mb(n: u64) -> f64 {
    n as f64 / 1048576.0
}

impl Ui {
    fn new() -> Ui {
        match control_fd() {
            Some(fd) => Ui::Gui { fd, open: Cell::new(false) },
            None => Ui::Tty { last_label: RefCell::new(String::new()), last_update: Cell::new(None) },
        }
    }

    fn emit(&self, message: Value) {
        if let Ui::Gui { fd, .. } = self {
            write_line(*fd, &message.to_string());
        }
    }

    fn configure(&self, label: &str) {
        self.emit(json!({
            "type": "configure", "kind": "frame", "from": DIALOG, "to": ["$term"],
            "label": label, "focus": true,
        }));
    }

    /// Declares the cell and loads its page, the first time.
    fn open(&self, label: &str) {
        let Ui::Gui { open, .. } = self else { return };
        if open.replace(true) {
            return;
        }
        self.configure(label);
        self.command(json!({ "type": "register_schemas", "schemas": {
            "cq-install:init": "(d) => cqInstall.init(d)",
            "cq-install:progress": "(d) => cqInstall.progress(d)",
            "cq-install:message": "(d) => cqInstall.message(d)",
        }}));
        self.command(json!({ "type": "html", "content": DIALOG_HTML }));
    }

    fn command(&self, message: Value) {
        self.emit(json!({ "type": "send", "from": DIALOG, "to": ["$term"], "message": message }));
    }

    /// A `data` command, dispatched in the page on its `$schema` (see
    /// register_schemas in `open`).
    fn data(&self, data: Value) {
        self.command(json!({ "type": "data", "data": data }));
    }

    /// Asks whether to install; true to go ahead.
    fn confirm(&self, host: &str, target: &str) -> bool {
        let Ui::Gui { fd, .. } = self else {
            return confirm_tty(&format!(
                "cq is not installed on {host}. Install coquille v{VERSION} ({target}) into ~/.local/bin there?"
            ));
        };
        self.open(&format!("+ssh {host}"));
        self.data(json!({ "$schema": "cq-install:init", "host": host, "version": format!("v{VERSION}"), "target": target }));
        // Wait for a click (other messages, if any, are not ours).
        while let Some(line) = read_line(*fd) {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message["type"] != "data" {
                continue;
            }
            match message["data"]["action"].as_str() {
                Some("install") => return true,
                Some("cancel") => return false,
                _ => {}
            }
        }
        false
    }

    /// Shows a stage of the installation, with how far along it is, when
    /// that is known.
    fn progress(&self, label: &str, done: Option<u64>, total: Option<u64>) {
        match self {
            Ui::Gui { .. } => {
                self.data(json!({ "$schema": "cq-install:progress", "label": label, "done": done, "total": total }))
            }
            Ui::Tty { last_label, last_update } => {
                let mut last = last_label.borrow_mut();
                let finished = matches!((done, total), (Some(d), Some(t)) if d >= t);
                let new_stage = *last != label;
                // At most ten updates a second, except to show a new stage or its end.
                if !new_stage && !finished && last_update.get().is_some_and(|t| t.elapsed() < THROTTLE) {
                    return;
                }
                last_update.set(Some(Instant::now()));
                if new_stage && !last.is_empty() {
                    eprintln!();
                }
                *last = label.to_owned();
                match (done, total) {
                    (Some(d), Some(t)) if t > 0 => {
                        let width = 30;
                        let filled = ((d.min(t) * width) / t) as usize;
                        let bar = "#".repeat(filled) + &"-".repeat(width as usize - filled);
                        eprint!("\r+ssh: {label} [{bar}] {:.1}/{:.1} MB", mb(d), mb(t));
                    }
                    _ => eprint!("\r+ssh: {label}"),
                }
                let _ = std::io::stderr().flush();
            }
        }
    }

    /// Closes the cell, if any (it stays, finished, above the session).
    fn finish(&self) {
        match self {
            Ui::Gui { open, .. } => {
                if open.get() {
                    self.emit(json!({
                        "type": "close", "from": DIALOG, "to": ["$term"], "outcome": { "type": "success" },
                    }));
                }
            }
            Ui::Tty { last_label, .. } => {
                if !last_label.borrow().is_empty() {
                    eprintln!();
                    last_label.borrow_mut().clear();
                }
            }
        }
    }

    /// Drops what the terminal still has to say about the cell (its resizes,
    /// say) once it is closed, so that it does not go on to the remote cq,
    /// which takes the channel over. Only the cell's messages come this way:
    /// cq handles those for the process's own terminal itself. Waits until
    /// the channel has been quiet for a moment (at most a second).
    fn drain(&self) {
        let Ui::Gui { fd, open } = self else { return };
        if !open.get() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let mut pfd = libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 150) } <= 0 || read_line(*fd).is_none() {
                return;
            }
        }
    }

    /// Shows a final message (made to stay) and exits with `code`.
    fn message(&self, title: &str, detail: &str, code: i32) -> ! {
        let error = code != 0;
        match self {
            Ui::Gui { open, .. } => {
                let label = if error { "+ssh: error" } else { "+ssh" };
                if open.get() {
                    self.configure(label);
                } else {
                    self.open(label);
                }
                self.data(json!({ "$schema": "cq-install:message", "title": title, "detail": detail, "error": error }));
            }
            Ui::Tty { .. } => {
                self.finish();
                let text = if detail.is_empty() { title.to_owned() } else { format!("{title}\n{detail}") };
                if error {
                    eprintln!("+ssh: {text}");
                } else {
                    println!("{text}");
                }
            }
        }
        exit(code)
    }

    fn fail(&self, title: impl std::fmt::Display, detail: impl std::fmt::Display) -> ! {
        self.message(&title.to_string(), &detail.to_string(), 1)
    }
}

fn confirm_tty(question: &str) -> bool {
    // Ask on the terminal itself, whatever stdin is.
    let Ok(tty) = fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return false;
    };
    let mut out = &tty;
    let _ = write!(out, "{question} [Y/n] ");
    let _ = out.flush();
    let mut answer = String::new();
    if BufReader::new(&tty).read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
}

/// The size of what `url` points to (after redirections), if the server
/// says.
fn content_length(url: &str) -> Option<u64> {
    let out = quiet(Command::new("curl").args(["-sfIL", url])).output().ok()?;
    let headers = String::from_utf8_lossy(&out.stdout);
    headers.lines().rev().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
    })
}

/// Downloads `url` to `dest`, showing progress under `label`.
fn curl(ui: &Ui, label: &str, url: &str, dest: &Path) {
    let total = content_length(url);
    let mut child = quiet(Command::new("curl").args(["-sSfL", "-o"]).arg(dest).arg(url))
        .spawn()
        .unwrap_or_else(|e| ui.fail("Could not run curl", e));
    let status = loop {
        let done = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        ui.progress(label, Some(done), total);
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(THROTTLE),
            Err(e) => ui.fail("curl failed", e),
        }
    };
    if !status.success() {
        ui.fail("Download failed", format!("{url}\n{}", stderr_of(&mut child)));
    }
    if let (Ok(meta), Some(total)) = (fs::metadata(dest), total) {
        ui.progress(label, Some(meta.len()), Some(total));
    }
}

/// Downloads the release archive for `target` into `dir` and checks its
/// sha256. Returns the archive's path.
fn download(ui: &Ui, target: &str, dir: &Path) -> PathBuf {
    let name = format!("coquille-{target}.tar.xz");
    let base = format!("{REPOSITORY}/releases/download/v{VERSION}");
    let archive = dir.join(&name);
    let sumfile = dir.join(format!("{name}.sha256"));
    curl(ui, &format!("Downloading {name}"), &format!("{base}/{name}"), &archive);
    ui.progress("Verifying checksum", None, None);
    curl(ui, "Verifying checksum", &format!("{base}/{name}.sha256"), &sumfile);

    let expected = fs::read_to_string(&sumfile)
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_lowercase))
        .unwrap_or_else(|| ui.fail("Could not read the checksum file", sumfile.display()));
    let data = fs::read(&archive).unwrap_or_else(|e| ui.fail(format!("Could not read {name}"), e));
    let actual = hex::encode(Sha256::digest(&data));
    if actual != expected {
        ui.fail(format!("Checksum mismatch for {name}"), format!("expected {expected}\ngot      {actual}"));
    }
    archive
}

/// Streams the archive to the remote, which unpacks it into ~/.local/bin.
fn upload(ui: &Ui, host: &str, archive: &Path) {
    let data = fs::read(archive).unwrap_or_else(|e| ui.fail(format!("Could not read {}", archive.display()), e));
    let total = data.len() as u64;
    let label = format!("Installing into ~/.local/bin on {host}");
    let mut child = quiet(ssh(host).arg(shell("sh", INSTALL)))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| ui.fail("Could not run ssh", e));
    let mut stdin = child.stdin.take().unwrap();
    let mut sent = 0u64;
    for chunk in data.chunks(64 * 1024) {
        ui.progress(&label, Some(sent), Some(total));
        if stdin.write_all(chunk).is_err() {
            break; // The remote gave up: its status says why.
        }
        sent += chunk.len() as u64;
    }
    ui.progress(&label, Some(sent), Some(total));
    drop(stdin);
    // Its output is small (errors, if anything): no risk of filling the pipes.
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    if !ok {
        ui.fail(format!("Installation on {host} failed"), stderr_of(&mut child));
    }
}

fn install(ui: &Ui, host: &str, target: &str) {
    let dir = env::temp_dir().join(format!("cq-ssh-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap_or_else(|e| ui.fail(format!("Could not create {}", dir.display()), e));
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let archive = download(ui, target, &dir);
    upload(ui, host, &archive);
}

/// The local stdembed: the one next to this executable, else from PATH.
fn local_stdembed() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("stdembed")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("stdembed"))
}

fn main() {
    let ui = Ui::new();
    let args: Vec<String> = env::args().skip(1).collect();
    let host = match &args[..] {
        [h] if h == "-h" || h == "--help" => ui.message("+ssh", USAGE, 0),
        [h] if !h.starts_with('-') => h.clone(),
        _ => ui.fail("Bad arguments", USAGE),
    };

    let (os, arch, status) = probe(&ui, &host);
    let run = match status {
        Status::OnPath => RUN_ON_PATH,
        Status::Local => RUN_LOCAL,
        Status::Missing => {
            let Some(target) = target_for(&os, &arch) else {
                ui.fail(
                    format!("cq is not installed on {host}"),
                    format!("There is no prebuilt release of coquille for {os}/{arch}."),
                );
            };
            if !ui.confirm(&host, target) {
                ui.finish();
                exit(1);
            }
            install(&ui, &host, target);
            ui.finish();
            ui.drain();
            RUN_LOCAL
        }
    };

    let err = Command::new(local_stdembed())
        .arg("-u")
        .arg("ssh")
        .args(control_opts())
        .arg("-t")
        .arg(&host)
        .arg(shell("bash -l", run))
        .exec();
    ui.fail("Could not run stdembed", err)
}
