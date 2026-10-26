use std::path::Path;

use capricorn_core::Secret;
use capricorn_core::framing::{read_msg, write_msg};
use capricorn_core::paths;
use capricorn_core::proto::{ClientReq, ClientResp, Status};
use tokio::net::UnixStream;

use crate::error::{CliError, Result};

/// One connection to the daemon. The daemon ties the unlocked session to the connection.
pub struct DaemonClient {
    stream: UnixStream,
}

impl DaemonClient {
    pub async fn connect() -> Result<Self> {
        Self::connect_to(&paths::client_socket()).await
    }

    pub async fn connect_to(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|source| CliError::Unreachable {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self { stream })
    }

    pub async fn request(&mut self, req: &ClientReq) -> Result<ClientResp> {
        write_msg(&mut self.stream, req)
            .await
            .map_err(lost_daemon)?;
        read_msg(&mut self.stream).await.map_err(lost_daemon)
    }

    /// Like `request`, but a daemon `Err` reply becomes a `CliError`.
    pub async fn call(&mut self, req: &ClientReq) -> Result<ClientResp> {
        match self.request(req).await? {
            ClientResp::Err(message) if message == "locked" => Err(CliError::Locked(
                "vault is locked; run `capricorn unlock`".into(),
            )),
            ClientResp::Err(message) => Err(CliError::Failed(message)),
            resp => Ok(resp),
        }
    }

    pub async fn call_ok(&mut self, req: &ClientReq) -> Result<()> {
        match self.call(req).await? {
            ClientResp::Ok => Ok(()),
            _ => Err(unexpected_reply()),
        }
    }

    pub async fn status(&mut self) -> Result<Status> {
        match self.call(&ClientReq::Status).await? {
            ClientResp::Status(status) => Ok(status),
            _ => Err(unexpected_reply()),
        }
    }

    pub async fn unlock(&mut self, password: Secret) -> Result<()> {
        self.request_with_password(&ClientReq::Unlock { password })
            .await
    }

    /// For requests whose only failure is a rejected password.
    pub async fn request_with_password(&mut self, req: &ClientReq) -> Result<()> {
        match self.request(req).await? {
            ClientResp::Ok => Ok(()),
            ClientResp::Err(message) => Err(CliError::Locked(message)),
            _ => Err(unexpected_reply()),
        }
    }
}

pub fn unexpected_reply() -> CliError {
    CliError::failed("unexpected reply from daemon")
}

fn lost_daemon(err: std::io::Error) -> CliError {
    CliError::Failed(format!("lost connection to capricornd: {err}"))
}
