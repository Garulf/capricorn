use std::path::PathBuf;
use std::sync::Arc;

use capricorn_core::framing::{read_msg, write_msg};
use capricorn_core::proto::{ClientReq, ClientResp};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// The daemon ties authentication to one connection, so every request shares one stream.
/// When it breaks, the next request opens a new one: a new session, which starts locked.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    path: PathBuf,
    conn: Arc<Mutex<Conn>>,
}

#[derive(Debug, Default)]
struct Conn {
    stream: Option<UnixStream>,
    session: u64,
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub session: u64,
    pub resp: ClientResp,
}

impl DaemonClient {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            conn: Arc::default(),
        }
    }

    pub async fn request(&self, req: ClientReq) -> Result<Reply, String> {
        let mut conn = self.conn.lock().await;
        if conn.stream.is_none() {
            let stream = UnixStream::connect(&self.path)
                .await
                .map_err(|e| format!("cannot reach capricornd: {e}"))?;
            conn.stream = Some(stream);
            conn.session += 1;
        }
        let session = conn.session;
        let stream = conn.stream.as_mut().expect("connected above");
        match exchange(stream, &req).await {
            Ok(resp) => Ok(Reply { session, resp }),
            Err(e) => {
                conn.stream = None;
                Err(format!("lost connection to capricornd: {e}"))
            }
        }
    }
}

async fn exchange(stream: &mut UnixStream, req: &ClientReq) -> std::io::Result<ClientResp> {
    write_msg(stream, req).await?;
    read_msg(stream).await
}

/// Which connection, if any, this UI unlocked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Session {
    authed: Option<u64>,
}

impl Session {
    pub fn unlocked_on(&mut self, session: u64) {
        self.authed = Some(session);
    }

    pub fn lock(&mut self) {
        self.authed = None;
    }

    pub fn is_unlocked(&self) -> bool {
        self.authed.is_some()
    }

    /// False, and locked from now on, when the reply came from another connection or the
    /// daemon refused it as locked.
    pub fn accepts(&mut self, reply: &Reply) -> bool {
        let refused = matches!(&reply.resp, ClientResp::Err(e) if e == "locked");
        if self.authed != Some(reply.session) || refused {
            self.authed = None;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_core::proto::{Status, VaultState};
    use tokio::net::UnixListener;

    fn status() -> ClientResp {
        ClientResp::Status(Status {
            vault: VaultState::Locked,
            recorder: None,
            pending_index: 0,
        })
    }

    async fn serve_one_request_per_connection(listener: UnixListener) {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: ClientReq = read_msg(&mut stream).await.unwrap();
            write_msg(&mut stream, &status()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn reconnect_starts_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(serve_one_request_per_connection(listener));
        let client = DaemonClient::new(path);

        let first = client.request(ClientReq::Status).await.unwrap();
        assert_eq!(first.resp, status());
        assert!(client.request(ClientReq::Status).await.is_err());
        let third = client.request(ClientReq::Status).await.unwrap();
        assert_eq!(third.session, first.session + 1);
    }

    #[tokio::test]
    async fn missing_socket_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = DaemonClient::new(dir.path().join("nope.sock"));
        let err = client.request(ClientReq::Status).await.unwrap_err();
        assert!(err.contains("cannot reach"), "{err}");
    }

    #[test]
    fn session_rejects_other_connection_and_locked_replies() {
        let reply = |session, resp| Reply { session, resp };
        let mut session = Session::default();
        assert!(!session.accepts(&reply(1, ClientResp::Ok)));

        session.unlocked_on(1);
        assert!(session.accepts(&reply(1, ClientResp::Ok)));
        assert!(!session.accepts(&reply(2, ClientResp::Ok)));
        assert!(!session.is_unlocked());

        session.unlocked_on(2);
        assert!(!session.accepts(&reply(2, ClientResp::Err("locked".into()))));
        assert!(!session.is_unlocked());
    }
}
