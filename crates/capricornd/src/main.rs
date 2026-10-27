use anyhow::Result;
use capricorn_core::{config, paths};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    capricornd::server::harden_process();
    let cfg = config::load(&paths::config_path())?;
    capricornd::run(cfg).await
}
