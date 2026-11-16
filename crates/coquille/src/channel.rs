//! buche's control channel, as the shell and its companion tools speak on it:
//! JSON lines, on the fd $BUCHE_CONTROL_FD says.

use std::os::fd::RawFd;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// The control channel's fd, as $BUCHE_CONTROL_FD says (open or not).
pub fn control_fd_var() -> Option<RawFd> {
    std::env::var("BUCHE_CONTROL_FD").ok()?.parse().ok()
}

/// The control channel, if there is one: $BUCHE_CONTROL_FD's, if it is open
/// (outside of buche, the variable may be inherited without the fd).
pub fn control_fd() -> Option<RawFd> {
    let fd = control_fd_var()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(fd)
}

/// Wait until `fd` is ready for `events` (POLLIN, POLLOUT...).
pub fn wait_fd(fd: RawFd, events: libc::c_short) {
    let mut pfd = libc::pollfd { fd, events, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, -1) };
}

/// Write one line on a file descriptor (which may be non-blocking): `line`,
/// without its trailing newlines, then one. A write that fails (but for
/// EAGAIN or EINTR) drops what is left.
pub fn write_line(fd: RawFd, line: &str) {
    let mut data = line.trim_end_matches('\n').as_bytes().to_vec();
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

/// Hand each JSON line of `input` to `handle`, in order, until its end, or
/// until `handle` says to stop (false). Lines that aren't JSON are skipped.
pub async fn json_lines<F, Fut>(input: impl AsyncRead + Unpin, mut handle: F)
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Ok(value) = serde_json::from_slice::<Value>(&line)
                    && !handle(value).await
                {
                    return;
                }
            }
        }
    }
}
