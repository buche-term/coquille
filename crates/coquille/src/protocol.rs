//! The messages coquille and its companion tools build, in one place: what
//! goes up to the terminal (`["$term"]`), and what a frame is sent. Their
//! shapes are buche's driver protocol (see ~/bu/buche: src/driver-exchange,
//! and its `incoming.schema.json`); messages relayed as they come are not
//! built here.

use serde_json::{Value, json};

use crate::message::NotImplemented;
use crate::util::merged;

/// The shell's own address.
pub const SELF: [&str; 2] = ["$proc", "cq"];

/// The terminal's address.
pub const TERM: [&str; 1] = ["$term"];

/// The address of the shell's process `id`.
pub fn process_addr(id: &str) -> Vec<String> {
    vec!["$proc".into(), "cq".into(), id.into()]
}

/// The address of `view` (`$stdout`, `$frame`, `?`) of the shell's process
/// `id`.
pub fn view_addr(id: &str, view: &str) -> Vec<String> {
    let mut addr = process_addr(id);
    addr.push(view.into());
    addr
}

/// A message of type `kind` from `from` to the terminal, with `fields` (an
/// object, or null for none).
pub fn to_term(kind: &str, from: impl Into<Value>, fields: Value) -> Value {
    merged(json!({ "type": kind, "from": from.into(), "to": TERM }), fields)
}

/// A `configure` of the entry at `from`.
pub fn configure(from: impl Into<Value>, fields: Value) -> Value {
    to_term("configure", from, fields)
}

/// A `send` of `message` (a command for the view: see html, data...) to the
/// view at `from`.
pub fn send(from: impl Into<Value>, message: Value) -> Value {
    to_term("send", from, json!({ "message": message }))
}

/// A `reset` of the entry at `from` (blank again).
pub fn reset(from: impl Into<Value>) -> Value {
    to_term("reset", from, Value::Null)
}

/// How something ended, for a `close`.
pub fn outcome(success: bool, code: Option<i64>) -> Value {
    let mut outcome = json!({ "type": if success { "success" } else { "error" } });
    if let Some(code) = code {
        outcome["code"] = json!(code);
    }
    outcome
}

/// A `close` of the entry at `from`, with its outcome (see outcome).
pub fn close(from: impl Into<Value>, outcome: Value) -> Value {
    to_term("close", from, json!({ "outcome": outcome }))
}

/// The kind of an error, as the terminal is told it (`error_type`).
fn error_type(err: &anyhow::Error) -> String {
    if err.downcast_ref::<NotImplemented>().is_some() {
        return "FeatureNotImplemented".into();
    }
    if let Some(code) = err.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()) {
        return format!("{:?}", nix::errno::Errno::from_raw(code));
    }
    "Error".into()
}

/// An `error` from the shell, from an error.
pub fn error(err: &anyhow::Error) -> Value {
    json!({
        "type": "error",
        "to": TERM,
        "from": SELF,
        "error_type": error_type(err),
        "message": err.to_string(),
        "traceback": err.chain().skip(1).map(|e| e.to_string()).collect::<Vec<_>>(),
    })
}

/// An `error` from the shell, of type `error_type`, without a traceback.
pub fn error_of_type(error_type: &str, message: &str) -> Value {
    to_term(
        "error",
        SELF,
        json!({ "error_type": error_type, "message": message, "traceback": [] }),
    )
}

/// A `configure` of the shell's prompt, with `fields` (an object, or null).
pub fn prompt_configure(fields: Value) -> Value {
    merged(
        json!({ "type": "configure", "kind": "prompt", "from": SELF, "to": TERM }),
        fields,
    )
}

/// A notification for the shell's daemons (sent to them as to `["$shell"]`).
pub fn to_shell(event: Value) -> Value {
    merged(json!({ "to": ["$shell"] }), event)
}

// The commands a frame is sent (see send).

/// Load `content` (a page) in the frame.
pub fn html(content: &str) -> Value {
    json!({ "type": "html", "content": content })
}

/// Hand the page `data` (dispatched on its `$schema`: see register_schemas).
pub fn data(data: Value) -> Value {
    json!({ "type": "data", "data": data })
}

/// Have the page handle the `data` of each `$schema` with its function
/// (JavaScript source, `(data) => ...`).
pub fn register_schemas(schemas: Value) -> Value {
    json!({ "type": "register_schemas", "schemas": schemas })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let close = close(process_addr("3"), outcome(false, Some(2)));
        assert_eq!(
            close.to_string(),
            r#"{"type":"close","from":["$proc","cq","3"],"to":["$term"],"outcome":{"type":"error","code":2}}"#
        );
        assert_eq!(
            reset(["$proc"]).to_string(),
            r#"{"type":"reset","from":["$proc"],"to":["$term"]}"#
        );
        assert_eq!(
            prompt_configure(json!({ "focus": true })).to_string(),
            r#"{"type":"configure","kind":"prompt","from":["$proc","cq"],"to":["$term"],"focus":true}"#
        );
        assert_eq!(
            send(view_addr("1", "$frame"), data(json!(1))).to_string(),
            r#"{"type":"send","from":["$proc","cq","1","$frame"],"to":["$term"],"message":{"type":"data","data":1}}"#
        );
    }
}
