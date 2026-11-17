//! The builtins: commands the shell runs itself. Each says what it asks the
//! shell to do (a BuiltinEvent: see Shell::perform); control commands ask the
//! same with messages to ["$shell"] (see BuiltinEvent::from_message).

use serde_json::Value;

pub const BUILTIN_NAMES: &[&str] = &["cd", "clear", "set", "export", "nuke", "daemon"];

pub fn is_builtin(name: &str) -> bool {
    BUILTIN_NAMES.contains(&name)
}

/// What a builtin asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum BuiltinEvent {
    Cd { path: String },
    Clear,
    Set { name: String, value: String, export: bool },
    Nuke,
    DaemonSet { name: String, cmd: String, args: Vec<String>, restart_ms: Option<u64> },
    DaemonEnable { name: String },
    DaemonDisable { name: String },
}

impl BuiltinEvent {
    /// What a control command's message to ["$shell"] asks for, if it is
    /// one of the builtin requests: `cd` {path}, `clear`, `set` {name, value,
    /// export?}, `nuke`, `daemon_set` {name, cmd, args?, restartMs?},
    /// `daemon_enable` {name}, `daemon_disable` {name}. (A missing text
    /// field is empty; one that isn't a text is taken as its JSON.)
    pub fn from_message(msg: &Value) -> Option<BuiltinEvent> {
        let text = |value: &Value| value.as_str().map_or_else(|| value.to_string(), str::to_owned);
        let field = |name: &str| match msg.get(name) {
            Some(Value::Null) | None => String::new(),
            Some(value) => text(value),
        };
        Some(match msg.get("type").and_then(Value::as_str)? {
            "cd" => BuiltinEvent::Cd { path: field("path") },
            "clear" => BuiltinEvent::Clear,
            "set" => BuiltinEvent::Set {
                name: field("name"),
                value: field("value"),
                export: msg.get("export") == Some(&Value::Bool(true)),
            },
            "nuke" => BuiltinEvent::Nuke,
            "daemon_set" => BuiltinEvent::DaemonSet {
                name: field("name"),
                cmd: field("cmd"),
                args: msg.get("args").and_then(Value::as_array).map_or(vec![], |a| a.iter().map(text).collect()),
                restart_ms: msg.get("restartMs").and_then(Value::as_u64),
            },
            "daemon_enable" => BuiltinEvent::DaemonEnable { name: field("name") },
            "daemon_disable" => BuiltinEvent::DaemonDisable { name: field("name") },
            _ => return None,
        })
    }

    /// Whether, asked for by a message, it holds the shell's input up until
    /// it is done (see Shell::receive), rather than run as a task alongside
    /// what comes next. (Kept as it was when these requests were dispatched
    /// by name: cd, clear and set run as tasks.)
    pub fn holds_input(&self) -> bool {
        !matches!(self, BuiltinEvent::Cd { .. } | BuiltinEvent::Clear | BuiltinEvent::Set { .. })
    }
}

fn assignments(args: &[String], export: bool) -> Vec<BuiltinEvent> {
    args.iter()
        .filter_map(|arg| arg.split_once('='))
        .map(|(name, value)| BuiltinEvent::Set { name: name.into(), value: value.into(), export })
        .collect()
}

/// What builtin `name` asks for, given `args`. Errors are usage errors.
pub fn builtin_events(name: &str, args: &[String]) -> anyhow::Result<Vec<BuiltinEvent>> {
    Ok(match name {
        "cd" => {
            let path = args.first().cloned().unwrap_or_else(|| crate::util::home().to_string_lossy().into_owned());
            vec![BuiltinEvent::Cd { path }]
        }
        "clear" => {
            if !args.is_empty() {
                anyhow::bail!("usage: clear");
            }
            vec![BuiltinEvent::Clear]
        }
        "set" => assignments(args, false),
        "export" => assignments(args, true),
        "nuke" => vec![BuiltinEvent::Nuke],
        "daemon" => {
            let usage = || anyhow::anyhow!("usage: daemon set NAME [-tMS] COMMAND... | daemon enable NAME | daemon disable NAME");
            let (sub, name, rest) = match args {
                [sub, name, rest @ ..] => (sub.as_str(), name.clone(), rest),
                _ => return Err(usage()),
            };
            match sub {
                "set" => {
                    let (restart_ms, rest) = match rest.first().and_then(|a| a.strip_prefix("-t")) {
                        Some(ms) => (Some(ms.parse().map_err(|_| anyhow::anyhow!("daemon set: bad delay {ms:?}"))?), &rest[1..]),
                        None => (None, rest),
                    };
                    let [cmd, args @ ..] = rest else { return Err(usage()) };
                    vec![BuiltinEvent::DaemonSet { name, cmd: cmd.clone(), args: args.to_vec(), restart_ms }]
                }
                "enable" => vec![BuiltinEvent::DaemonEnable { name }],
                "disable" => vec![BuiltinEvent::DaemonDisable { name }],
                _ => return Err(usage()),
            }
        }
        _ => anyhow::bail!("not a builtin: {name}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parses() {
        assert_eq!(
            builtin_events("export", &s(&["A=1", "B", "C=x=y"])).unwrap(),
            vec![
                BuiltinEvent::Set { name: "A".into(), value: "1".into(), export: true },
                BuiltinEvent::Set { name: "C".into(), value: "x=y".into(), export: true },
            ]
        );
        assert_eq!(
            builtin_events("daemon", &s(&["set", "v", "-t500", "watch", "-x"])).unwrap(),
            vec![BuiltinEvent::DaemonSet { name: "v".into(), cmd: "watch".into(), args: s(&["-x"]), restart_ms: Some(500) }]
        );
        assert!(builtin_events("daemon", &s(&["set", "v"])).is_err());
        assert_eq!(builtin_events("clear", &s(&[])).unwrap(), vec![BuiltinEvent::Clear]);
        assert!(builtin_events("clear", &s(&["-x"])).is_err());
    }

    #[test]
    fn parses_messages() {
        use serde_json::json;
        assert_eq!(
            BuiltinEvent::from_message(&json!({"type": "set", "name": "A", "value": 3, "export": true})),
            Some(BuiltinEvent::Set { name: "A".into(), value: "3".into(), export: true })
        );
        let daemon = json!({"type": "daemon_set", "name": "v", "cmd": "w", "args": ["-x", 1], "restartMs": 5});
        let expected =
            BuiltinEvent::DaemonSet { name: "v".into(), cmd: "w".into(), args: s(&["-x", "1"]), restart_ms: Some(5) };
        assert_eq!(BuiltinEvent::from_message(&daemon), Some(expected));
        assert_eq!(BuiltinEvent::from_message(&json!({"type": "cd"})), Some(BuiltinEvent::Cd { path: "".into() }));
        assert_eq!(BuiltinEvent::from_message(&json!({"type": "clear"})), Some(BuiltinEvent::Clear));
        assert_eq!(BuiltinEvent::from_message(&json!({"type": "parse"})), None);
    }
}
