//! Periodic work: idle lock, sealing finished hours, retention and index snapshots.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, MissedTickBehavior, interval, interval_at};
use tracing::warn;

use crate::daemon::Daemon;

const LONGEST_IDLE_CHECK: Duration = Duration::from_secs(5);
const SEAL_EVERY: Duration = Duration::from_secs(60);
const RETENTION_EVERY: Duration = Duration::from_secs(60 * 60);
const SNAPSHOT_EVERY: Duration = Duration::from_secs(10 * 60);

/// Runs until the task is aborted.
pub async fn run_timers(daemon: Arc<Daemon>) {
    let mut idle = every(idle_check_period(daemon.config().lock_timeout_secs));
    let mut seal = every(SEAL_EVERY);
    let mut retention = every(RETENTION_EVERY);
    let mut snapshot = interval_at(Instant::now() + SNAPSHOT_EVERY, SNAPSHOT_EVERY);
    snapshot.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = idle.tick() => run_job(&daemon, "idle lock", Daemon::lock_if_idle).await,
            _ = seal.tick() => run_job(&daemon, "seal", Daemon::seal_due).await,
            _ = retention.tick() => run_job(&daemon, "retention", Daemon::apply_retention).await,
            _ = snapshot.tick() => run_job(&daemon, "snapshot", Daemon::write_snapshot).await,
        }
    }
}

/// Checks often enough that a short timeout is honoured within about a second.
fn idle_check_period(lock_timeout_secs: u64) -> Duration {
    Duration::from_secs(lock_timeout_secs.max(1)).min(LONGEST_IDLE_CHECK)
}

fn every(period: Duration) -> tokio::time::Interval {
    let mut timer = interval(period);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    timer
}

async fn run_job(daemon: &Arc<Daemon>, name: &'static str, job: fn(&Daemon) -> anyhow::Result<()>) {
    let daemon = daemon.clone();
    match tokio::task::spawn_blocking(move || job(&daemon)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(
            job = name,
            error = format!("{error:#}"),
            "periodic job failed"
        ),
        Err(error) => warn!(job = name, %error, "periodic job panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_check_tracks_short_timeouts() {
        assert_eq!(idle_check_period(0), Duration::from_secs(1));
        assert_eq!(idle_check_period(2), Duration::from_secs(2));
        assert_eq!(idle_check_period(900), LONGEST_IDLE_CHECK);
    }
}
