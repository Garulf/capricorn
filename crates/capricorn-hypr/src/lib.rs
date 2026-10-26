//! Hyprland IPC client, event parser and reopen command builder.

mod error;
mod events;
mod ipc;
mod model;
mod reopen;

pub use error::{Error, Result};
pub use events::{Event, parse_event};
pub use ipc::Hypr;
pub use model::{Client, Monitor, WsRef};
pub use reopen::{ReopenPlan, ReopenSpec, reopen_command, reopen_plan};
