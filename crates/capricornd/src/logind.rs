//! Lock triggers from systemd-logind: suspend and session screen locks.

use std::future::Future;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};
use zbus::message::Type;
use zbus::{Connection, MatchRule, Message, MessageStream};
use zbus::{OwnedFd, OwnedObjectPath};

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockCause {
    Sleep,
    ScreenLock,
}

/// A lock the vault must perform. `done` fires once the vault is locked, or is dropped
/// if locking could not run.
pub struct LockRequest {
    pub cause: LockCause,
    pub done: oneshot::Sender<()>,
}

impl LockRequest {
    pub fn new(cause: LockCause) -> (Self, oneshot::Receiver<()>) {
        let (done, locked) = oneshot::channel();
        (Self { cause, done }, locked)
    }
}

/// A source of events that must lock the vault.
pub trait LockSignals: Send + 'static {
    /// Sends a request for every event until the source ends or `tx` closes.
    fn watch(self, tx: mpsc::Sender<LockRequest>) -> impl Future<Output = ()> + Send;
}

/// For tests and hosts without logind.
pub struct NoLockSignals;

impl LockSignals for NoLockSignals {
    async fn watch(self, _tx: mpsc::Sender<LockRequest>) {}
}

/// `PrepareForSleep(true)` and the `Lock` signal of any session owned by an allowed
/// uid (any uid when the list is empty). Holds a logind delay inhibitor for sleep, so
/// suspend waits until the vault is locked, and reconnects if the bus goes away.
pub struct Logind {
    pub allowed_uids: Vec<u32>,
}

enum Event {
    Lock(LockCause),
    Resumed,
}

impl LockSignals for Logind {
    async fn watch(self, tx: mpsc::Sender<LockRequest>) {
        let mut retry = FIRST_RETRY;
        while !tx.is_closed() {
            match self.listen(&tx, &mut retry).await {
                Ok(()) if tx.is_closed() => return,
                Ok(()) => warn!("logind signal stream ended; reconnecting"),
                Err(error) => warn!(
                    %error,
                    "logind is unavailable; suspend and screen lock cannot lock the vault until it returns"
                ),
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(MAX_RETRY);
        }
    }
}

impl Logind {
    async fn listen(
        &self,
        tx: &mpsc::Sender<LockRequest>,
        retry: &mut Duration,
    ) -> zbus::Result<()> {
        let conn = Connection::system().await?;
        let sleep = signal_stream(&conn, MANAGER, "PrepareForSleep").await?;
        let locks = signal_stream(&conn, SESSION, "Lock").await?;
        let mut signals = futures::stream::select(sleep, locks);
        let mut inhibitor = take_inhibitor(&conn).await;
        while let Some(msg) = signals.next().await {
            *retry = FIRST_RETRY;
            let event = match self.classify(&conn, &msg?).await {
                Ok(event) => event,
                Err(error) => {
                    warn!(%error, "ignoring unreadable logind signal");
                    None
                }
            };
            match event {
                Some(Event::Lock(cause)) => {
                    let (request, locked) = LockRequest::new(cause);
                    if tx.send(request).await.is_err() {
                        break;
                    }
                    if cause == LockCause::Sleep {
                        let _ = locked.await;
                        inhibitor = None;
                    }
                }
                Some(Event::Resumed) if inhibitor.is_none() => {
                    inhibitor = take_inhibitor(&conn).await;
                }
                Some(Event::Resumed) | None => {}
            }
        }
        drop(inhibitor);
        Ok(())
    }

    async fn classify(&self, conn: &Connection, msg: &Message) -> zbus::Result<Option<Event>> {
        let header = msg.header();
        match header.member().map(|member| member.as_str()) {
            Some("PrepareForSleep") => {
                let going_down: bool = msg.body().deserialize()?;
                Ok(Some(if going_down {
                    Event::Lock(LockCause::Sleep)
                } else {
                    Event::Resumed
                }))
            }
            Some("Lock") => {
                let Some(path) = header.path() else {
                    return Ok(None);
                };
                let uid = session_uid(conn, path.as_str()).await?;
                let allowed = uid.is_some_and(|uid| {
                    self.allowed_uids.is_empty() || self.allowed_uids.contains(&uid)
                });
                Ok(allowed.then_some(Event::Lock(LockCause::ScreenLock)))
            }
            _ => Ok(None),
        }
    }
}

/// Without the inhibitor (polkit can refuse it) suspend still locks the vault, but may
/// freeze the daemon before the lock finishes.
async fn take_inhibitor(conn: &Connection) -> Option<OwnedFd> {
    let taken = async {
        let reply = conn
            .call_method(
                Some(LOGIND),
                MANAGER_PATH,
                Some(MANAGER),
                "Inhibit",
                &("sleep", "capricorn", "lock vault before sleep", "delay"),
            )
            .await?;
        reply.body().deserialize::<OwnedFd>()
    };
    match taken.await {
        Ok(fd) => {
            info!("holding a logind sleep delay inhibitor");
            Some(fd)
        }
        Err(error) => {
            warn!(%error, "could not take a logind sleep inhibitor; suspend may not wait for the lock");
            None
        }
    }
}

async fn signal_stream(
    conn: &Connection,
    interface: &'static str,
    member: &'static str,
) -> zbus::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(LOGIND)
        .interface(interface)
        .member(member)
        .build()?;
    MessageStream::for_match_rule(rule, conn, None).await
}

async fn session_uid(conn: &Connection, session_path: &str) -> zbus::Result<Option<u32>> {
    let reply = conn
        .call_method(
            Some(LOGIND),
            MANAGER_PATH,
            Some(MANAGER),
            "ListSessions",
            &(),
        )
        .await?;
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        reply.body().deserialize()?;
    Ok(sessions
        .into_iter()
        .find(|(_, _, _, _, path)| path.as_str() == session_path)
        .map(|(_, uid, _, _, _)| uid))
}
