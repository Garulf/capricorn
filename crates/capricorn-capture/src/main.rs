//! User-session capture agent: grabs screens and clipboard with Hyprland context and
//! streams them to capricornd.

mod clipboard;
mod config;
mod context;
mod cwd;
mod exclude;
mod focus;
mod idle;
mod inject;
mod link;
mod lock;
mod recorder;
mod scheduler;
mod screencopy;
mod wl;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rustix::process::{DumpableBehavior, set_dumpable_behavior};
use tracing::warn;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Inject(inject::InjectArgs),
}

/// The daemon trusts this binary by its path, so a release build only ever talks to the
/// system socket; otherwise anyone could start it with their own socket and receive frames.
/// Debug builds honour `CAPRICORN_RUNTIME_DIR` for tests.
pub fn daemon_socket() -> PathBuf {
    if cfg!(debug_assertions) {
        capricorn_core::paths::capture_socket()
    } else {
        capricorn_core::paths::default_capture_socket()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = set_dumpable_behavior(DumpableBehavior::NotDumpable) {
        warn!("cannot mark the process undumpable: {e}");
    }
    match Cli::parse().command {
        Some(Command::Inject(args)) => inject::run(args).await,
        None => recorder::run().await,
    }
}
