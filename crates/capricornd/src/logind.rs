//! Lock triggers from systemd-logind: suspend and session screen locks.

use std::future::Future;

use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;
use zbus::OwnedObjectPath;
use zbus::message::Type;
use zbus::{Connection, MatchRule, Message, MessageStream};

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockCause {
    Sleep,
    ScreenLock,
}

/// A source of events that must lock the vault.
pub trait LockSignals: Send + 'static {
    /// Sends a cause for every event until the source ends or `tx` closes.
    fn watch(self, tx: mpsc::Sender<LockCause>) -> impl Future<Output = ()> + Send;
}

/// For tests and hosts without logind.
pub struct NoLockSignals;

impl LockSignals for NoLockSignals {
    async fn watch(self, _tx: mpsc::Sender<LockCause>) {}
}

/// `PrepareForSleep(true)` and the `Lock` signal of any session owned by an allowed
/// uid (any uid when the list is empty).
pub struct Logind {
    pub allowed_uids: Vec<u32>,
}

impl LockSignals for Logind {
    async fn watch(self, tx: mpsc::Sender<LockCause>) {
        if let Err(error) = self.listen(&tx).await {
            warn!(%error, "logind is unavailable; suspend and screen lock will not lock the vault");
        }
    }
}

impl Logind {
    async fn listen(&self, tx: &mpsc::Sender<LockCause>) -> zbus::Result<()> {
        let conn = Connection::system().await?;
        let sleep = signal_stream(&conn, MANAGER, "PrepareForSleep").await?;
        let locks = signal_stream(&conn, SESSION, "Lock").await?;
        let mut signals = futures::stream::select(sleep, locks);
        while let Some(msg) = signals.next().await {
            let cause = match self.classify(&conn, &msg?).await {
                Ok(cause) => cause,
                Err(error) => {
                    warn!(%error, "ignoring unreadable logind signal");
                    None
                }
            };
            if let Some(cause) = cause
                && tx.send(cause).await.is_err()
            {
                break;
            }
        }
        Ok(())
    }

    async fn classify(&self, conn: &Connection, msg: &Message) -> zbus::Result<Option<LockCause>> {
        let header = msg.header();
        match header.member().map(|member| member.as_str()) {
            Some("PrepareForSleep") => {
                let going_down: bool = msg.body().deserialize()?;
                Ok(going_down.then_some(LockCause::Sleep))
            }
            Some("Lock") => {
                let Some(path) = header.path() else {
                    return Ok(None);
                };
                let uid = session_uid(conn, path.as_str()).await?;
                let allowed = uid.is_some_and(|uid| {
                    self.allowed_uids.is_empty() || self.allowed_uids.contains(&uid)
                });
                Ok(allowed.then_some(LockCause::ScreenLock))
            }
            _ => Ok(None),
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
