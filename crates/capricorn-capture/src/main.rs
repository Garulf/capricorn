//! User-session capture agent: grabs screens and clipboard with Hyprland context and
//! streams them to capricornd.

mod clipboard;
mod config;
mod context;
mod cwd;
mod exclude;
mod idle;
mod inject;
mod link;
mod recorder;
mod scheduler;
mod screencopy;
mod wl;

use clap::{Parser, Subcommand};
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Some(Command::Inject(args)) => inject::run(args).await,
        None => recorder::run().await,
    }
}
