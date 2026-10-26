//! Session lock detection. Lockers on Hyprland rarely report to logind, so a running
//! screen locker of ours counts as locked too.

use std::path::Path;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use zbus::{Connection, OwnedObjectPath, OwnedVariant};

use crate::scheduler::Input;

const POLL: Duration = Duration::from_secs(1);
const LOGIND_RETRY: Duration = Duration::from_secs(30);
const LOCKERS: &[&str] = &["hyprlock", "swaylock", "gtklock", "waylock"];

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Sends `Locked` whenever the lock state changes, starting with the current one.
pub async fn watch(tx: UnboundedSender<Input>) {
    let uid = rustix::process::getuid().as_raw();
    let mut logind = LockedHint::default();
    let mut ticks = tokio::time::interval(POLL);
    let mut reported = None;
    while !tx.is_closed() {
        ticks.tick().await;
        let locker = tokio::task::spawn_blocking(move || locker_running(Path::new("/proc"), uid))
            .await
            .unwrap_or(false);
        let locked = locker || logind.locked().await;
        if reported != Some(locked) {
            debug!(locked, "session lock state");
            reported = Some(locked);
            let _ = tx.send(Input::Locked(locked));
        }
    }
}

/// True when a process owned by `uid` under `proc_root` is a known screen locker.
fn locker_running(proc_root: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()));
        is_pid
            && entry.metadata().is_ok_and(|m| m.uid() == uid)
            && std::fs::read_to_string(entry.path().join("comm"))
                .is_ok_and(|comm| LOCKERS.contains(&comm.trim_end()))
    })
}

/// logind's `LockedHint` on the user's graphical session, read over the system bus.
#[derive(Default)]
struct LockedHint {
    session: Option<Session>,
    retry_at: Option<Instant>,
    warned: bool,
}

struct Session {
    conn: Connection,
    path: OwnedObjectPath,
}

impl LockedHint {
    async fn locked(&mut self) -> bool {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        let session = match self.session.take() {
            Some(session) => Ok(session),
            None => Session::connect().await,
        };
        let read = match session {
            Ok(session) => session.locked_hint().await.map(|locked| (session, locked)),
            Err(error) => Err(error),
        };
        match read {
            Ok((session, locked)) => {
                self.session = Some(session);
                locked
            }
            Err(error) => {
                if !std::mem::replace(&mut self.warned, true) {
                    warn!(%error, "cannot read logind LockedHint; relying on locker processes");
                }
                self.retry_at = Some(Instant::now() + LOGIND_RETRY);
                false
            }
        }
    }
}

impl Session {
    /// `auto` is the caller's session, or the user's graphical session for a user service.
    async fn connect() -> zbus::Result<Self> {
        let conn = Connection::system().await?;
        let reply = conn
            .call_method(
                Some(LOGIND),
                MANAGER_PATH,
                Some(MANAGER),
                "GetSession",
                &("auto",),
            )
            .await?;
        let path: OwnedObjectPath = reply.body().deserialize()?;
        Ok(Self { conn, path })
    }

    async fn locked_hint(&self) -> zbus::Result<bool> {
        let reply = self
            .conn
            .call_method(
                Some(LOGIND),
                &self.path,
                Some(PROPERTIES),
                "Get",
                &(SESSION, "LockedHint"),
            )
            .await?;
        let value: OwnedVariant = reply.body().deserialize()?;
        bool::try_from(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(root: &Path, pid: &str, comm: &str) {
        let dir = root.join(pid);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
    }

    fn my_uid() -> u32 {
        rustix::process::getuid().as_raw()
    }

    #[test]
    fn finds_a_running_locker() {
        let root = tempfile::tempdir().unwrap();
        process(root.path(), "12", "kitty");
        assert!(!locker_running(root.path(), my_uid()));
        process(root.path(), "345", "hyprlock");
        assert!(locker_running(root.path(), my_uid()));
    }

    #[test]
    fn ignores_non_pid_entries_and_other_users() {
        let root = tempfile::tempdir().unwrap();
        process(root.path(), "self", "swaylock");
        process(root.path(), "1x", "swaylock");
        assert!(!locker_running(root.path(), my_uid()));
        process(root.path(), "77", "swaylock");
        assert!(!locker_running(root.path(), my_uid().wrapping_add(1)));
    }

    #[test]
    fn locker_names_must_match_exactly() {
        let root = tempfile::tempdir().unwrap();
        process(root.path(), "9", "hyprlock-helper");
        process(root.path(), "10", "notswaylock");
        assert!(!locker_running(root.path(), my_uid()));
    }

    #[test]
    fn missing_proc_is_unlocked() {
        assert!(!locker_running(Path::new("/nonexistent/proc"), my_uid()));
    }
}
