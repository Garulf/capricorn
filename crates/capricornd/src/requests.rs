//! Client requests and their replies.

use capricorn_core::Secret;
use capricorn_core::proto::{ClientReq, ClientResp, SearchQuery};
use tracing::warn;

use crate::daemon::{Credential, Daemon, DaemonError};
use crate::state::ConnId;

const MAX_SEARCH_RESULTS: u32 = 1000;

pub struct Reply {
    pub resp: ClientResp,
    /// The request failed a password check; the caller waits before replying.
    pub penalty: bool,
}

impl Reply {
    pub fn internal_error() -> Self {
        Self {
            resp: ClientResp::Err("internal error".into()),
            penalty: false,
        }
    }

    fn from_error(error: DaemonError) -> Self {
        let penalty = matches!(error, DaemonError::BadPassword | DaemonError::EmptyPassword);
        match error {
            DaemonError::Internal(error) => {
                warn!(error = format!("{error:#}"), "request failed");
                Self::internal_error()
            }
            other => Self {
                resp: ClientResp::Err(other.to_string()),
                penalty,
            },
        }
    }
}

impl Daemon {
    pub fn handle(&self, conn: ConnId, req: ClientReq) -> Reply {
        match self.dispatch(conn, req) {
            Ok(resp) => Reply {
                resp,
                penalty: false,
            },
            Err(error) => Reply::from_error(error),
        }
    }

    fn dispatch(&self, conn: ConnId, req: ClientReq) -> Result<ClientResp, DaemonError> {
        let resp = match req {
            ClientReq::Status => ClientResp::Status(self.status(conn)),
            ClientReq::Init { password, recovery } => {
                match self.init(password.expose(), recovery)? {
                    Some(code) => ClientResp::RecoveryCode(Secret::new(code)),
                    None => ClientResp::Ok,
                }
            }
            ClientReq::Unlock { password } => {
                self.unlock(conn, Credential::Password(password.expose()))?;
                ClientResp::Ok
            }
            ClientReq::UnlockRecovery { code } => {
                self.unlock(conn, Credential::RecoveryCode(code.expose()))?;
                ClientResp::Ok
            }
            ClientReq::Lock => {
                self.lock_vault("requested");
                ClientResp::Ok
            }
            ClientReq::ChangePassword { old, new } => {
                self.change_password(conn, old.expose(), new.expose())?;
                ClientResp::Ok
            }
            ClientReq::Search(query) => ClientResp::Hits(
                self.with_session(conn, |unlocked| {
                    Ok(unlocked.index.search(&capped(query))?)
                })?,
            ),
            ClientReq::Timeline { from, to } => ClientResp::Timeline(
                self.with_session(conn, |unlocked| Ok(unlocked.index.timeline(from, to)?))?,
            ),
            ClientReq::Detail(r) => ClientResp::Detail(self.with_session(conn, |unlocked| {
                unlocked.index.detail(r)?.ok_or(DaemonError::NotFound)
            })?),
            ClientReq::Image { r, thumb } => ClientResp::Image(self.image(conn, r, thumb)?),
            ClientReq::Delete(scope) => {
                self.delete(conn, scope)?;
                ClientResp::Ok
            }
            ClientReq::Forget { minutes } => {
                self.forget(minutes)?;
                ClientResp::Ok
            }
            ClientReq::Pause => {
                self.set_paused(true);
                ClientResp::Ok
            }
            ClientReq::Resume => {
                self.set_paused(false);
                ClientResp::Ok
            }
            ClientReq::Touch => {
                self.touch(conn);
                ClientResp::Ok
            }
        };
        Ok(resp)
    }
}

fn capped(mut query: SearchQuery) -> SearchQuery {
    query.limit = query.limit.min(MAX_SEARCH_RESULTS);
    query
}
