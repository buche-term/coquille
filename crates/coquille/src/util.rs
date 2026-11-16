//! Small helpers: paths, executables, terminal sizes, hashes, JSON objects,
//! addresses, UTF-16 offsets.

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The user's home directory.
pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The home directory of `user`, if there is such a user.
pub fn user_home(user: &str) -> Option<PathBuf> {
    nix::unistd::User::from_name(user).ok().flatten().map(|u| u.dir)
}

/// The working directory (`/` if it can't be had).
pub fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// `path` made absolute against `base` and normalized lexically (`.` and
/// `..` resolved, symlinks left alone), as node's `path.resolve`.
pub fn resolve(base: &Path, path: impl AsRef<Path>) -> PathBuf {
    let joined = base.join(path);
    let mut out = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// `~` and `~/...` expanded to the home directory.
pub fn expand_home(word: &str) -> String {
    if word == "~" {
        home().to_string_lossy().into_owned()
    } else if let Some(rest) = word.strip_prefix("~/") {
        home().join(rest).to_string_lossy().into_owned()
    } else {
        word.to_owned()
    }
}

/// `~` and `~user` at the start of a path expanded to the home directory
/// (the user's), as Python's `os.path.expanduser` does it: an unknown user
/// is left as is.
pub fn expand_user(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_owned();
    };
    let (user, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let home = if user.is_empty() { Some(home()) } else { user_home(user) };
    match home.map(|h| h.to_string_lossy().into_owned()) {
        Some(home) if tail.is_empty() => home,
        Some(home) => format!("{}{tail}", home.trim_end_matches('/')),
        None => path.to_owned(),
    }
}

/// A path with the home directory as `~`.
pub fn tildify(file: &str) -> String {
    let home = home();
    let home = home.to_string_lossy();
    if file == home {
        return "~".into();
    }
    match file.strip_prefix(&*home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => file.to_owned(),
    }
}

/// Whether `path` is a regular file this user may execute.
pub fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            meta.permissions().mode() & 0o111 != 0 && nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
        }
        _ => false,
    }
}

/// The directories of a PATH, in order, as a POSIX shell (and execvp)
/// searches them: relative to the working directory `cwd`, an empty entry
/// being the working directory itself.
pub fn path_dirs<'a>(path: &'a str, cwd: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
    path.split(':')
        .map(move |dir| resolve(cwd, if dir.is_empty() { "." } else { dir }))
}

/// The file `cmd` runs, as execvp finds it (in `path`, from `cwd`): the
/// first executable of that name in the PATH's directories (see path_dirs),
/// or, if it has a slash, the file it names (whatever it is).
pub fn find_executable(cmd: &str, path: &str, cwd: &Path) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Some(resolve(cwd, cmd));
    }
    path_dirs(path, cwd)
        .map(|dir| dir.join(cmd))
        .find(|candidate| is_executable(candidate))
}

/// The size (columns, rows) of a terminal whose size isn't known.
pub const DEFAULT_TERMINAL_SIZE: (u16, u16) = (80, 24);

/// The size buche gives its commands' terminals ($BUCHE_PTY_COLS,
/// $BUCHE_PTY_ROWS): columns and rows, each if set to a positive number.
pub fn buche_terminal_size() -> (Option<u16>, Option<u16>) {
    let var = |name| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|&n: &u16| n > 0)
    };
    (var("BUCHE_PTY_COLS"), var("BUCHE_PTY_ROWS"))
}

/// The size (columns, rows) of the terminal on `fd`, if it is one (each 0
/// if it doesn't know). Async-signal-safe.
pub fn tty_size(fd: std::os::fd::RawFd) -> Option<(u16, u16)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    (unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0).then_some((ws.ws_col, ws.ws_row))
}

/// The size (columns, rows) of the terminal commands get, dimension by
/// dimension: buche's (see buche_terminal_size), else that of the terminal
/// on `fd`, else DEFAULT_TERMINAL_SIZE.
pub fn terminal_size(fd: std::os::fd::RawFd) -> (u16, u16) {
    let (cols, rows) = buche_terminal_size();
    let (tty_cols, tty_rows) = tty_size(fd).unwrap_or((0, 0));
    let pick = |env: Option<u16>, tty: u16, default| env.or((tty > 0).then_some(tty)).unwrap_or(default);
    (
        pick(cols, tty_cols, DEFAULT_TERMINAL_SIZE.0),
        pick(rows, tty_rows, DEFAULT_TERMINAL_SIZE.1),
    )
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The length of `s` in UTF-16 code units (JavaScript's `length`: what the
/// terminal counts selections in).
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The byte offset in `s` of UTF-16 offset `n` (clamped to the text, and
/// down to a character boundary).
pub fn utf16_to_byte(s: &str, n: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= n {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

/// Describe a JSON value's kind, for error messages.
pub fn describe(x: &Value) -> &'static str {
    match x {
        Value::Null => "null",
        Value::Array(_) => "a list",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
    }
}

/// `base` with the fields of `extra` set over it (JavaScript's `{...base,
/// ...extra}`: a field already there keeps its place).
pub fn merged(base: Value, extra: Value) -> Value {
    match (base, extra) {
        (Value::Object(mut a), Value::Object(b)) => {
            for (k, v) in b {
                a.insert(k, v);
            }
            Value::Object(a)
        }
        (a, Value::Null) => a,
        (_, b) => b,
    }
}

/// Insert `key: value` in `obj`, if `value` is some.
pub fn put_opt(obj: &mut Map<String, Value>, key: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        obj.insert(key.to_owned(), value.into());
    }
}

/// The segments of an address (a JSON array of strings), if it is one.
pub fn address_of(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|s| s.as_str().map(str::to_owned))
        .collect()
}

/// A relative address (["$proc", ...rest]: a process's, as it says it)
/// qualified under `prefix` (where it is: [...prefix, ...rest]). None if it
/// isn't relative.
pub fn qualify(address: &[String], prefix: &[String]) -> Option<Vec<String>> {
    let rest = address.strip_prefix(&["$proc".to_owned()])?;
    Some(prefix.iter().chain(rest).cloned().collect())
}

/// The address `rest` under a process, relative to it (["$proc", ...rest]:
/// as the process knows it).
pub fn relative(rest: &[String]) -> Vec<String> {
    std::iter::once("$proc".to_owned())
        .chain(rest.iter().cloned())
        .collect()
}

/// An address under `prefix` (a process's), relative to it (see relative).
/// None if it isn't under it.
pub fn unqualify(address: &[String], prefix: &[String]) -> Option<Vec<String>> {
    Some(relative(address.strip_prefix(prefix)?))
}

/// Incremental UTF-8 decoding of a byte stream: a character split between
/// two reads is decoded whole, with the second.
#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(), // invalid, not merely cut: replacement characters
        };
        let rest = self.pending.split_off(valid);
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending = rest;
        text
    }

    /// What is left at the end of the stream.
    pub fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths() {
        assert_eq!(resolve(Path::new("/a/b"), "../c/./d"), PathBuf::from("/a/c/d"));
        assert_eq!(resolve(Path::new("/a"), "/x/../y"), PathBuf::from("/y"));
        assert_eq!(resolve(Path::new("/"), ".."), PathBuf::from("/"));
    }

    #[test]
    fn expands_users() {
        let home = home().to_string_lossy().into_owned();
        assert_eq!(expand_user("~"), home);
        assert_eq!(expand_user("~/x"), format!("{}/x", home.trim_end_matches('/')));
        assert_eq!(expand_user("~nosuchuserxyz/x"), "~nosuchuserxyz/x");
        assert_eq!(expand_user("a/~"), "a/~");
    }

    #[test]
    fn path_dirs_like_posix() {
        let dirs: Vec<PathBuf> = path_dirs("/bin::sub", Path::new("/w")).collect();
        assert_eq!(
            dirs,
            vec![PathBuf::from("/bin"), PathBuf::from("/w"), PathBuf::from("/w/sub")]
        );
    }

    #[test]
    fn qualifies_addresses() {
        let a = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            qualify(&a(&["$proc", "x"]), &a(&["$proc", "cq", "3"])),
            Some(a(&["$proc", "cq", "3", "x"]))
        );
        assert_eq!(qualify(&a(&["$term"]), &a(&["$proc", "cq", "3"])), None);
        assert_eq!(
            unqualify(&a(&["$proc", "cq", "3", "x"]), &a(&["$proc", "cq", "3"])),
            Some(a(&["$proc", "x"]))
        );
        assert_eq!(unqualify(&a(&["$proc", "cq", "4"]), &a(&["$proc", "cq", "3"])), None);
    }

    #[test]
    fn utf16_offsets() {
        let s = "a😀b";
        assert_eq!(utf16_len(s), 4);
        assert_eq!(utf16_to_byte(s, 1), 1);
        assert_eq!(utf16_to_byte(s, 3), 5);
        assert_eq!(utf16_to_byte(s, 9), 6);
    }

    #[test]
    fn decodes_split_characters() {
        let mut d = Utf8Decoder::default();
        let bytes = "é!".as_bytes();
        assert_eq!(d.decode(&bytes[..1]), "");
        assert_eq!(d.decode(&bytes[1..]), "é!");
    }
}
