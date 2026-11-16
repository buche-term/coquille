//! coquille: the shell (`cq`: see main.rs, and Shell), and what its companion
//! tools (src/utilities/: sel, stdembed, +ssh, watch) share with it: the
//! control channel (see channel.rs) and small helpers (see util.rs).

mod approval;
mod builtins;
pub mod channel;
mod config;
mod config_watch;
mod expand;
pub mod fdio;
mod highlight;
mod history;
mod library;
mod look;
mod message;
mod process;
mod prompt;
pub mod protocol;
mod requests;
mod shell;
mod trust;
pub mod util;
pub mod words;

pub use shell::Shell;
