use std::time::Duration;

use capricorn_core::framing::{read_msg, write_msg};
use capricorn_core::proto::{CaptureCtl, CaptureMsg};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedSender};
use tracing::{info, warn};

/// Small on purpose: frames are large and stale ones are worthless.
const QUEUE_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    Connected,
    Ctl(CaptureCtl),
}

/// Connection to capricornd that reconnects with backoff. Messages sent while disconnected
/// or while the queue is full are dropped, never buffered.
pub struct DaemonLink {
    tx: Sender<CaptureMsg>,
}

impl DaemonLink {
    pub fn spawn(events: UnboundedSender<LinkEvent>) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        tokio::spawn(run(rx, events));
        Self { tx }
    }

    /// Returns false when the message was dropped.
    pub fn send(&self, msg: CaptureMsg) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

async fn run(mut rx: Receiver<CaptureMsg>, events: UnboundedSender<LinkEvent>) {
    let mut backoff = Backoff::default();
    while !rx.is_closed() {
        let socket = crate::daemon_socket();
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                info!("connected to {}", socket.display());
                backoff = Backoff::default();
                let _ = events.send(LinkEvent::Connected);
                if let Err(e) = serve(stream, &mut rx, &events).await {
                    warn!("daemon connection lost: {e}");
                }
            }
            Err(e) => warn!("cannot reach {}: {e}", socket.display()),
        }
        drop_messages_for(&mut rx, backoff.next_delay()).await;
    }
}

async fn serve(
    stream: UnixStream,
    rx: &mut Receiver<CaptureMsg>,
    events: &UnboundedSender<LinkEvent>,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let read_ctl = async {
        loop {
            let ctl: CaptureCtl = read_msg(&mut reader).await?;
            let _ = events.send(LinkEvent::Ctl(ctl));
        }
    };
    tokio::pin!(read_ctl);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => write_msg(&mut writer, &msg).await?,
                None => return Ok(()),
            },
            result = &mut read_ctl => return result,
        }
    }
}

async fn drop_messages_for(rx: &mut Receiver<CaptureMsg>, delay: Duration) {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return,
            msg = rx.recv() => if msg.is_none() { return },
        }
    }
}

#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Backoff {
    const FIRST: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(30);

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        delay
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: Self::FIRST }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut backoff = Backoff::default();
        let delays: Vec<_> = (0..9).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(
            delays,
            [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]
        );
    }
}
