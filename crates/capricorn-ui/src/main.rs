mod app;
mod client;
mod control;
mod exclusions;
mod format;
mod icons;
mod keys;
mod lanes;
mod reopen;
mod search;
mod style;
mod timeline;
mod unlock;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod view_tests;

use anyhow::Context;
use rustix::process::{DumpableBehavior, set_dumpable_behavior};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    set_dumpable_behavior(DumpableBehavior::NotDumpable).context("PR_SET_DUMPABLE")?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let socket = control::socket_path().context("XDG_RUNTIME_DIR is not set")?;
    let listener =
        control::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let client = client::DaemonClient::new(capricorn_core::paths::client_socket());
    let never_recorded = exclusions::never_recorded(exclusions::capture_config_path().as_deref());
    app::run(client, never_recorded, listener).context("running the layer-shell UI")
}
