//! Messages, as JSON values: those received (from the terminal, a child, or
//! routed internally to `["$shell"]`), and those emitted. The protocol is
//! buche's driver protocol (see ~/bu/buche: src/driver-exchange); most
//! messages are relayed as they are, so they are kept as JSON throughout.
//! Those the shell builds itself are built in protocol.rs.

use serde_json::Value;

pub type Message = Value;

/// A feature of the shell language that coquille doesn't run (yet).
#[derive(Debug)]
pub struct NotImplemented(pub String);

impl std::fmt::Display for NotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Feature not implemented: {}", self.0)
    }
}

impl std::error::Error for NotImplemented {}

pub fn not_implemented(feature: impl Into<String>) -> anyhow::Error {
    NotImplemented(feature.into()).into()
}

/// A message's `type`.
pub fn kind(msg: &Message) -> &str {
    msg.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The first segment of a message's `to`.
pub fn to_head(msg: &Message) -> Option<&str> {
    msg.get("to")?.as_array()?.first()?.as_str()
}

/// The length of a message's `from` (what it says of where it comes from).
pub fn from_len(msg: &Message) -> Option<usize> {
    Some(msg.get("from")?.as_array()?.len())
}

/// The segment `i` of a message's `from`.
pub fn from_at(msg: &Message, i: usize) -> Option<&str> {
    msg.get("from")?.as_array()?.get(i)?.as_str()
}
