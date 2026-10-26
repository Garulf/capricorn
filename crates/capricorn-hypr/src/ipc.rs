use crate::events::{Event, parse_event};
use crate::{Error, Result};
use futures::Stream;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Hypr {
    sig: String,
    runtime: PathBuf,
}

impl Hypr {
    pub fn from_env() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| Error::MissingEnv("HYPRLAND_INSTANCE_SIGNATURE"))?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::MissingEnv("XDG_RUNTIME_DIR"))?;
        Ok(Self::new(runtime, sig))
    }

    pub fn new(runtime: impl Into<PathBuf>, sig: impl Into<String>) -> Self {
        Self {
            sig: sig.into(),
            runtime: runtime.into(),
        }
    }

    fn socket_path(&self, name: &str) -> PathBuf {
        self.runtime.join("hypr").join(&self.sig).join(name)
    }

    /// Sends `cmd` (for example `j/clients`) and parses the JSON reply.
    pub async fn query<T: DeserializeOwned>(&self, cmd: &str) -> Result<T> {
        let reply = self.request(cmd).await?;
        Ok(serde_json::from_str(&reply)?)
    }

    /// Sends a raw request such as `dispatch exec [workspace 3] kitty`.
    pub async fn dispatch(&self, args: &str) -> Result<()> {
        let reply = self.request(args).await?;
        match reply.trim() {
            "ok" => Ok(()),
            other => Err(Error::Rejected(other.to_owned())),
        }
    }

    /// Returns the focused window, or `None` when nothing is focused.
    pub async fn active_window(&self) -> Result<Option<crate::Client>> {
        let client: crate::Client = self.query("j/activewindow").await?;
        Ok((!client.is_empty()).then_some(client))
    }

    pub async fn clients(&self) -> Result<Vec<crate::Client>> {
        self.query("j/clients").await
    }

    pub async fn monitors(&self) -> Result<Vec<crate::Monitor>> {
        self.query("j/monitors").await
    }

    /// Streams parsed events until Hyprland closes the socket.
    pub async fn events(&self) -> Result<impl Stream<Item = Event>> {
        let stream = UnixStream::connect(self.socket_path(".socket2.sock")).await?;
        let lines = BufReader::new(stream).lines();
        Ok(futures::stream::unfold(lines, |mut lines| async move {
            let line = lines.next_line().await.ok()??;
            Some((parse_event(&line), lines))
        }))
    }

    async fn request(&self, cmd: &str) -> Result<String> {
        let exchange = async {
            let mut stream = UnixStream::connect(self.socket_path(".socket.sock")).await?;
            stream.write_all(cmd.as_bytes()).await?;
            let mut reply = Vec::new();
            stream.read_to_end(&mut reply).await?;
            Ok::<_, Error>(String::from_utf8_lossy(&reply).into_owned())
        };
        tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| Error::Timeout)?
    }
}
