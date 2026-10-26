//! Sockets, connection handling and daemon lifecycle.

use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use capricorn_core::config::Config;
use capricorn_core::framing::{read_msg, read_msg_limited, write_msg};
use capricorn_core::proto::{CaptureCtl, CaptureMsg, ClientReq, ClientResp};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::capture::{CaptureQueue, RateLimit, Work};
use crate::daemon::Daemon;
use crate::logind::{LockRequest, LockSignals, Logind, NoLockSignals};
use crate::ocr::{Ocr, OcrsEngine};
use crate::peer::check_peer;
use crate::pipeline::Pipeline;
use crate::requests::Reply;
use crate::state::ConnId;

pub const CAPTURE_SOCKET: &str = "capture.sock";
pub const CLIENT_SOCKET: &str = "client.sock";
const SOCKET_MODE: u32 = 0o660;
const OCR_MODELS_ENV: &str = "CAPRICORN_OCR_MODELS";
const FAILED_AUTH_DELAY: Duration = Duration::from_secs(1);
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
const FRAMES_PER_SEC: u32 = 2;
const CLIPBOARDS_PER_SEC: u32 = 10;
/// Per socket; further peers are turned away until one disconnects.
const MAX_CONNECTIONS: usize = 64;
/// Client requests are small; only capture frames need the full frame limit.
const CLIENT_FRAME_LIMIT: u32 = 64 * 1024;

/// The production entry point: OCR models from `CAPRICORN_OCR_MODELS`, lock signals
/// from logind, and SIGTERM or Ctrl-C to stop.
pub async fn run(cfg: Config) -> Result<()> {
    let models = std::env::var_os(OCR_MODELS_ENV)
        .map(PathBuf::from)
        .with_context(|| format!("{OCR_MODELS_ENV} is not set"))?;
    let ocr = tokio::task::spawn_blocking(move || OcrsEngine::load(&models)).await??;
    let logind = Logind {
        allowed_uids: cfg.allowed_uids.clone(),
    };
    run_with_lock_signals(cfg, Arc::new(ocr), logind, shutdown_signal()).await
}

/// Runs without logind until `shutdown` completes.
pub async fn run_with(
    cfg: Config,
    ocr: Arc<dyn Ocr>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    run_with_lock_signals(cfg, ocr, NoLockSignals, shutdown).await
}

pub async fn run_with_lock_signals(
    cfg: Config,
    ocr: Arc<dyn Ocr>,
    signals: impl LockSignals,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if cfg.allowed_uids.is_empty() && !cfg.allow_any_uid {
        bail!("allowed_uids is empty; list the users who may connect, or set allow_any_uid = true");
    }
    let secret_patterns = cfg.compiled_secret_patterns()?;
    let runtime_dir = cfg.runtime_dir();
    std::fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("creating {}", runtime_dir.display()))?;
    let daemon = Arc::new(Daemon::open(cfg)?);
    let queue = Arc::new(CaptureQueue::default());
    let worker = spawn_worker(
        Pipeline::new(ocr, secret_patterns),
        queue.clone(),
        daemon.clone(),
    )?;
    let capture_path = runtime_dir.join(CAPTURE_SOCKET);
    let client_path = runtime_dir.join(CLIENT_SOCKET);
    let capture = bind(&capture_path)?;
    let client = bind(&client_path)?;

    let mut tasks = JoinSet::new();
    tasks.spawn(accept_captures(capture, daemon.clone(), queue.clone()));
    tasks.spawn(accept_clients(client, daemon.clone()));
    tasks.spawn(crate::retention_task::run_timers(daemon.clone()));
    tasks.spawn(ingest_when_asked(daemon.clone()));
    tasks.spawn(lock_on_signals(signals, daemon.clone()));
    info!(runtime_dir = %runtime_dir.display(), "capricornd listening");

    shutdown.await;
    info!("shutting down");
    tasks.shutdown().await;
    queue.close();
    let _ = tokio::task::spawn_blocking(move || worker.join()).await;
    let locker = daemon.clone();
    let _ = tokio::task::spawn_blocking(move || locker.lock_vault("shutdown")).await;
    for path in [capture_path, client_path] {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Keeps the process out of core dumps and same-uid ptrace, and private by default.
pub fn harden_process() {
    // SAFETY: plain prctl and umask calls without pointers.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        libc::umask(0o077);
    }
}

async fn shutdown_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            warn!(%error, "cannot watch SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Replaces a stale socket file, then opens the socket to the group.
fn bind(path: &Path) -> Result<UnixListener> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("removing stale {}", path.display()));
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    Ok(listener)
}

fn spawn_worker(
    mut pipeline: Pipeline,
    queue: Arc<CaptureQueue>,
    daemon: Arc<Daemon>,
) -> Result<std::thread::JoinHandle<()>> {
    let worker = std::thread::Builder::new()
        .name("capture-worker".into())
        .spawn(move || {
            while let Some(work) = queue.next() {
                let record = match work {
                    Work::Frame(frame) => pipeline.frame(frame),
                    Work::Clipboard(entry) => Ok(pipeline.clipboard(entry)),
                };
                let stored = record.and_then(|record| match record {
                    Some(record) => daemon.store_capture(record),
                    None => Ok(()),
                });
                if let Err(error) = stored {
                    warn!(error = format!("{error:#}"), "dropping capture");
                }
            }
        })?;
    Ok(worker)
}

async fn accept_captures(listener: UnixListener, daemon: Arc<Daemon>, queue: Arc<CaptureQueue>) {
    let mut connections = JoinSet::new();
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut last_capture = 0u64;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Some(stream) = admit(accepted, &daemon, |cfg| &cfg.capture_clients).await else {
                    continue;
                };
                let Some(slot) = take_slot(&slots) else {
                    continue;
                };
                last_capture += 1;
                connections.spawn(serve_capture(stream, daemon.clone(), queue.clone(), last_capture, slot));
            }
            Some(_) = connections.join_next() => {}
        }
    }
}

async fn accept_clients(listener: UnixListener, daemon: Arc<Daemon>) {
    let mut connections = JoinSet::new();
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut last_conn: ConnId = 0;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Some(stream) = admit(accepted, &daemon, |cfg| &cfg.ui_clients).await else {
                    continue;
                };
                let Some(slot) = take_slot(&slots) else {
                    continue;
                };
                last_conn += 1;
                connections.spawn(serve_client(stream, daemon.clone(), last_conn, slot));
            }
            Some(_) = connections.join_next() => {}
        }
    }
}

fn take_slot(slots: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    let slot = slots.clone().try_acquire_owned().ok();
    if slot.is_none() {
        warn!(limit = MAX_CONNECTIONS, "too many connections; refused one");
    }
    slot
}

/// Peer checks fail closed: a peer whose identity cannot be read is refused.
async fn admit(
    accepted: io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
    daemon: &Daemon,
    allowed_exes: impl Fn(&Config) -> &Vec<PathBuf>,
) -> Option<UnixStream> {
    let stream = match accepted {
        Ok((stream, _)) => stream,
        Err(error) => {
            warn!(%error, "accept failed");
            tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            return None;
        }
    };
    let cfg = daemon.config();
    match check_peer(&stream, &cfg.allowed_uids, allowed_exes(cfg)) {
        Ok(()) => Some(stream),
        Err(error) => {
            warn!(error = format!("{error:#}"), "refused connection");
            None
        }
    }
}

async fn serve_client(
    mut stream: UnixStream,
    daemon: Arc<Daemon>,
    conn: ConnId,
    _slot: OwnedSemaphorePermit,
) {
    loop {
        let req: ClientReq = match read_msg_limited(&mut stream, CLIENT_FRAME_LIMIT).await {
            Ok(req) => req,
            Err(error) => {
                if error.kind() != io::ErrorKind::UnexpectedEof {
                    debug!(%error, "dropping client after a bad frame");
                }
                break;
            }
        };
        let resp = respond(&daemon, conn, req).await;
        if write_msg(&mut stream, &resp).await.is_err() {
            break;
        }
    }
    let _ = tokio::task::spawn_blocking(move || daemon.end_session(conn)).await;
}

async fn respond(daemon: &Arc<Daemon>, conn: ConnId, req: ClientReq) -> ClientResp {
    let handler = daemon.clone();
    let reply = tokio::task::spawn_blocking(move || handler.handle(conn, req))
        .await
        .unwrap_or_else(|_| Reply::internal_error());
    if reply.penalty {
        tokio::time::sleep(FAILED_AUTH_DELAY).await;
    }
    reply.resp
}

async fn serve_capture(
    stream: UnixStream,
    daemon: Arc<Daemon>,
    queue: Arc<CaptureQueue>,
    capture: u64,
    _slot: OwnedSemaphorePermit,
) {
    daemon.capture_connected(capture);
    let (mut reader, writer) = stream.into_split();
    let forwarder = tokio::spawn(forward_ctl(
        writer,
        daemon.subscribe_ctl(),
        daemon.is_paused(),
    ));
    let mut frames = RateLimit::new(FRAMES_PER_SEC);
    let mut clipboard = RateLimit::new(CLIPBOARDS_PER_SEC);
    loop {
        let msg: CaptureMsg = match read_msg(&mut reader).await {
            Ok(msg) => msg,
            Err(error) => {
                if error.kind() != io::ErrorKind::UnexpectedEof {
                    debug!(%error, "dropping capture client after a bad frame");
                }
                break;
            }
        };
        match msg {
            CaptureMsg::Frame(frame) => {
                if frames.allow() && daemon.accepts_captures() {
                    queue.push_frame(frame);
                }
            }
            CaptureMsg::Clipboard(entry) => {
                if clipboard.allow() && daemon.accepts_captures() {
                    queue.push_clipboard(entry);
                }
            }
            CaptureMsg::Forget { minutes } => {
                let forgetter = daemon.clone();
                let _ = tokio::task::spawn_blocking(move || forgetter.forget(minutes)).await;
            }
            CaptureMsg::Recorder(recorder) => daemon.set_recorder(capture, recorder),
        }
    }
    forwarder.abort();
    daemon.capture_disconnected(capture);
}

async fn forward_ctl(
    mut writer: OwnedWriteHalf,
    mut ctl: broadcast::Receiver<CaptureCtl>,
    paused: bool,
) {
    if paused && write_msg(&mut writer, &CaptureCtl::Pause).await.is_err() {
        return;
    }
    loop {
        match ctl.recv().await {
            Ok(msg) => {
                if write_msg(&mut writer, &msg).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn ingest_when_asked(daemon: Arc<Daemon>) {
    loop {
        daemon.catch_up_requested().await;
        let ingester = daemon.clone();
        match tokio::task::spawn_blocking(move || ingester.catch_up()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(error = format!("{error:#}"), "indexing failed"),
            Err(error) => warn!(%error, "indexing panicked"),
        }
    }
}

async fn lock_on_signals(signals: impl LockSignals, daemon: Arc<Daemon>) {
    let (tx, mut rx) = mpsc::channel(8);
    let locker = async {
        while let Some(LockRequest { cause, done }) = rx.recv().await {
            info!(?cause, "lock signal");
            let daemon = daemon.clone();
            let locked =
                tokio::task::spawn_blocking(move || daemon.lock_vault("system lock signal")).await;
            if locked.is_ok() {
                let _ = done.send(());
            }
        }
    };
    tokio::join!(signals.watch(tx), locker);
}
