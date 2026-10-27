//! Capture processing, the encrypted vault service and its sockets.

pub mod capture;
pub mod daemon;
pub mod dedupe;
pub mod index_store;
pub mod ingest;
pub mod logind;
pub mod ocr;
pub mod peer;
pub mod pipeline;
pub mod redact;
pub mod requests;
pub mod retention_task;
pub mod server;
pub mod state;

pub use server::{run, run_with, run_with_lock_signals};
