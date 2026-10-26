//! End-to-end tests of the daemon over its sockets, with a fake OCR engine.

use std::collections::HashMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capricorn_core::Secret;
use capricorn_core::config::{Config, KdfOverride};
use capricorn_core::framing::{read_msg, write_msg};
use capricorn_core::proto::{
    CaptureCtl, CaptureMsg, ClientReq, ClientResp, ClipboardEntry, DeleteScope, Frame, Hit, Kind,
    RecordRef, RecorderState, Rect, SearchQuery, Status, VaultState, WindowContext,
};
use capricornd::logind::{LockCause, LockRequest, LockSignals};
use capricornd::ocr::{Ocr, Word};
use image::RgbImage;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const PASSWORD: &str = "correct horse battery staple";
const WAIT_LIMIT: Duration = Duration::from_secs(60);
const FRAME_SPACING: Duration = Duration::from_millis(550);

/// Answers with the words taught for the colour of a frame's first pixel.
#[derive(Default)]
struct FakeOcr {
    texts: Mutex<HashMap<[u8; 3], String>>,
    calls: AtomicUsize,
}

impl FakeOcr {
    fn teach(&self, color: [u8; 3], text: &str) {
        self.texts.lock().unwrap().insert(color, text.into());
    }
}

impl Ocr for FakeOcr {
    fn words(&self, img: &RgbImage) -> anyhow::Result<Vec<Word>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let color = img.get_pixel(0, 0).0;
        let texts = self.texts.lock().unwrap();
        let Some(text) = texts.get(&color) else {
            return Ok(Vec::new());
        };
        Ok(text
            .split(' ')
            .enumerate()
            .map(|(i, word)| Word {
                text: word.into(),
                rect: Rect {
                    x: i as i32 * 12,
                    y: 4,
                    w: 10,
                    h: 8,
                },
            })
            .collect())
    }
}

struct Running {
    runtime_dir: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }

    async fn client(&self) -> Client {
        Client(connect(&self.runtime_dir.join("client.sock")).await)
    }

    async fn capture(&self, ocr: &Arc<FakeOcr>) -> Capture {
        Capture {
            stream: connect(&self.runtime_dir.join("capture.sock")).await,
            ocr: ocr.clone(),
        }
    }
}

fn test_config(root: &Path) -> Config {
    Config {
        data_dir: Some(root.join("data")),
        runtime_dir: Some(root.join("run")),
        allow_any_uid: true,
        kdf: Some(KdfOverride {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        }),
        ..Config::default()
    }
}

async fn start(cfg: Config, ocr: &Arc<FakeOcr>) -> Running {
    start_with(cfg, ocr, |cfg, ocr, stop| {
        tokio::spawn(capricornd::run_with(cfg, ocr, stop))
    })
    .await
}

async fn start_with<F>(cfg: Config, ocr: &Arc<FakeOcr>, launch: F) -> Running
where
    F: FnOnce(
        Config,
        Arc<dyn Ocr>,
        std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> JoinHandle<anyhow::Result<()>>,
{
    let runtime_dir = cfg.runtime_dir();
    let (shutdown, stopped) = oneshot::channel::<()>();
    let stop = Box::pin(async move {
        let _ = stopped.await;
    });
    let task = launch(cfg, ocr.clone(), stop);
    let socket = runtime_dir.join("client.sock");
    eventually("daemon socket", || {
        let socket = socket.clone();
        async move { UnixStream::connect(&socket).await.is_ok() }
    })
    .await;
    Running {
        runtime_dir,
        shutdown,
        task,
    }
}

async fn connect(path: &Path) -> UnixStream {
    UnixStream::connect(path).await.unwrap()
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = Instant::now();
    while !check().await {
        assert!(
            started.elapsed() < WAIT_LIMIT,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn secret(value: &str) -> Secret {
    Secret::new(value.into())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Built at runtime so the test source holds no single canary string.
fn canary(name: &str) -> String {
    format!("zq{name}{}", "canaryx")
}

fn locked() -> ClientResp {
    ClientResp::Err("locked".into())
}

struct Client(UnixStream);

impl Client {
    async fn call(&mut self, req: ClientReq) -> ClientResp {
        write_msg(&mut self.0, &req).await.unwrap();
        read_msg(&mut self.0).await.unwrap()
    }

    async fn status(&mut self) -> Status {
        match self.call(ClientReq::Status).await {
            ClientResp::Status(status) => status,
            other => panic!("expected status, got {other:?}"),
        }
    }

    async fn init(&mut self, password: &str) {
        let resp = self
            .call(ClientReq::Init {
                password: secret(password),
                recovery: false,
            })
            .await;
        assert_eq!(resp, ClientResp::Ok);
    }

    async fn unlock(&mut self, password: &str) -> ClientResp {
        self.call(ClientReq::Unlock {
            password: secret(password),
        })
        .await
    }

    async fn search(&mut self, text: &str) -> ClientResp {
        self.call(ClientReq::Search(SearchQuery {
            text: text.into(),
            kind: None,
            app: None,
            workspace: None,
            from: None,
            to: None,
            limit: 50,
        }))
        .await
    }

    async fn hits(&mut self, text: &str) -> Vec<Hit> {
        match self.search(text).await {
            ClientResp::Hits(hits) => hits,
            other => panic!("expected hits, got {other:?}"),
        }
    }

    async fn wait_for_hits(&mut self, text: &str, count: usize) -> Vec<Hit> {
        let started = Instant::now();
        loop {
            let hits = self.hits(text).await;
            if hits.len() == count {
                return hits;
            }
            assert!(
                started.elapsed() < WAIT_LIMIT,
                "expected {count} hits, have {}",
                hits.len()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_until_indexed(&mut self) {
        let started = Instant::now();
        while self.status().await.pending_index > 0 {
            assert!(started.elapsed() < WAIT_LIMIT, "indexing never finished");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_for_vault(&mut self, wanted: fn(&VaultState) -> bool) {
        self.wait_for_status(|status| wanted(&status.vault)).await;
    }

    async fn wait_for_status(&mut self, wanted: impl Fn(&Status) -> bool) {
        let started = Instant::now();
        while !wanted(&self.status().await) {
            assert!(started.elapsed() < WAIT_LIMIT, "status never changed");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

struct Capture {
    stream: UnixStream,
    ocr: Arc<FakeOcr>,
}

impl Capture {
    async fn send(&mut self, msg: CaptureMsg) {
        write_msg(&mut self.stream, &msg).await.unwrap();
    }

    /// Paced below the per-connection frame rate limit.
    async fn frame(&mut self, class: &str, text: &str) {
        self.frame_at(class, text, now_ms()).await;
    }

    async fn frame_at(&mut self, class: &str, text: &str, ts: i64) {
        let color = color_for(text);
        self.ocr.teach(color, text);
        let output = format!("OUT-{text}");
        let (width, height) = (64u32, 36u32);
        let rgba = [color[0], color[1], color[2], 255].repeat((width * height) as usize);
        self.send(CaptureMsg::Frame(Frame {
            ts,
            output: output.clone(),
            width,
            height,
            rgba,
            window: window(class, &output),
        }))
        .await;
        tokio::time::sleep(FRAME_SPACING).await;
    }

    async fn clipboard(&mut self, class: &str, text: &str) {
        self.send(CaptureMsg::Clipboard(ClipboardEntry {
            ts: now_ms(),
            text: text.into(),
            source: window(class, "DP-1"),
        }))
        .await;
    }
}

fn window(class: &str, output: &str) -> WindowContext {
    WindowContext {
        class: class.into(),
        title: format!("{class} window"),
        workspace: "1".into(),
        output: output.into(),
        pid: None,
        cwd: None,
    }
}

fn color_for(text: &str) -> [u8; 3] {
    let hash = text.bytes().fold(17u32, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u32::from(b))
    });
    let [a, b, c, _] = hash.to_le_bytes();
    [a, b, c]
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

fn assert_absent_on_disk(dir: &Path, needles: &[String]) {
    let files = files_under(dir);
    assert!(!files.is_empty(), "data dir is empty");
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        for needle in needles {
            let leaked = bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes());
            assert!(!leaked, "{} holds a canary in plaintext", path.display());
        }
    }
}

fn segment_bytes(data_dir: &Path) -> u64 {
    let segments = data_dir.join("segments");
    if !segments.exists() {
        return 0;
    }
    files_under(&segments)
        .iter()
        .map(|path| std::fs::metadata(path).map_or(0, |meta| meta.len()))
        .sum()
}

/// Waits until `count` frames went through OCR and the segment files stopped growing.
async fn wait_until_stored(data_dir: &Path, ocr: &FakeOcr, count: usize) {
    let started = Instant::now();
    while ocr.calls.load(Ordering::SeqCst) < count {
        assert!(started.elapsed() < WAIT_LIMIT, "frames never processed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut last = 0;
    eventually("segments settled", || {
        let now = segment_bytes(data_dir);
        let settled = now > 0 && now == last;
        last = now;
        async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            settled
        }
    })
    .await;
}

async fn initialized(cfg: Config, ocr: &Arc<FakeOcr>) -> (Running, Client) {
    let daemon = start(cfg, ocr).await;
    let mut client = daemon.client().await;
    client.init(PASSWORD).await;
    (daemon, client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_unlock_search_flow() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let daemon = start(test_config(root.path()), &ocr).await;
    let mut ui = daemon.client().await;

    assert_eq!(ui.status().await.vault, VaultState::Uninitialized);
    assert_eq!(
        ui.unlock(PASSWORD).await,
        ClientResp::Err("not initialized".into())
    );
    let code = match ui
        .call(ClientReq::Init {
            password: secret(PASSWORD),
            recovery: true,
        })
        .await
    {
        ClientResp::RecoveryCode(code) => code,
        other => panic!("expected a recovery code, got {other:?}"),
    };
    assert_eq!(ui.status().await.vault, VaultState::Locked);
    let again = ui
        .call(ClientReq::Init {
            password: secret(PASSWORD),
            recovery: false,
        })
        .await;
    assert_eq!(again, ClientResp::Err("already initialized".into()));

    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    let status = ui.status().await;
    assert!(matches!(status.vault, VaultState::Unlocked { locks_in_secs } if locks_in_secs > 0));

    let mut capture = daemon.capture(&ocr).await;
    capture.frame("kitty", "nebula lantern").await;
    capture.clipboard("firefox", "orchard ticket").await;
    let hits = ui.wait_for_hits("nebula", 1).await;
    assert_eq!(hits[0].app, "kitty");
    let clip = ui.wait_for_hits("orchard", 1).await;
    assert_eq!(clip[0].kind, Kind::Clipboard);

    match ui.call(ClientReq::Detail(hits[0].r)).await {
        ClientResp::Detail(detail) => assert!(detail.text.contains("lantern")),
        other => panic!("expected detail, got {other:?}"),
    }
    match ui
        .call(ClientReq::Image {
            r: hits[0].r,
            thumb: true,
        })
        .await
    {
        ClientResp::Image(bytes) => assert!(bytes.starts_with(b"RIFF")),
        other => panic!("expected an image, got {other:?}"),
    }
    match ui
        .call(ClientReq::Timeline {
            from: 0,
            to: i64::MAX,
        })
        .await
    {
        ClientResp::Timeline(entries) => assert_eq!(entries.len(), 2),
        other => panic!("expected a timeline, got {other:?}"),
    }
    let missing = RecordRef { hour: 1, seq: 9 };
    assert_eq!(
        ui.call(ClientReq::Detail(missing)).await,
        ClientResp::Err("not found".into())
    );

    let mut cli = daemon.client().await;
    let recovered = cli.call(ClientReq::UnlockRecovery { code }).await;
    assert_eq!(recovered, ClientResp::Ok);
    assert_eq!(cli.hits("nebula").await.len(), 1);

    let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(daemon.runtime_dir.join("client.sock")), 0o660);
    assert_eq!(mode(daemon.runtime_dir.join("capture.sock")), 0o660);
    assert_eq!(mode(root.path().join("data")), 0o700);
    daemon.stop().await;
    assert!(!root.path().join("run/client.sock").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_while_locked_searchable_after_unlock() {
    let root = TempDir::new().unwrap();
    let data_dir = root.path().join("data");
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let screen = canary("screen");
    let clip = canary("clip");
    let title = canary("title");

    let mut capture = daemon.capture(&ocr).await;
    capture.frame(&title, &format!("{screen} visible")).await;
    capture.clipboard("kitty", &format!("{clip} copied")).await;
    wait_until_stored(&data_dir, &ocr, 1).await;
    let needles = [screen.clone(), clip.clone(), title.clone()];
    assert_absent_on_disk(&data_dir, &needles);
    assert_eq!(ui.search(&screen).await, locked());

    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    assert_eq!(ui.wait_for_hits(&screen, 1).await[0].app, title);
    ui.wait_for_hits(&clip, 1).await;
    assert_absent_on_disk(&data_dir, &needles);

    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    assert_eq!(ui.status().await.vault, VaultState::Locked);
    assert!(data_dir.join("index.snap").exists());
    assert!(data_dir.join("index.key").exists());
    assert_absent_on_disk(&data_dir, &needles);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_connection_cannot_search() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut client) = initialized(test_config(root.path()), &ocr).await;
    let r = RecordRef { hour: 1, seq: 0 };
    let refused = [
        ClientReq::Search(SearchQuery {
            text: "anything".into(),
            kind: None,
            app: None,
            workspace: None,
            from: None,
            to: None,
            limit: 5,
        }),
        ClientReq::Timeline { from: 0, to: 1 },
        ClientReq::Detail(r),
        ClientReq::Image { r, thumb: false },
        ClientReq::Delete(DeleteScope::One(r)),
        ClientReq::Delete(DeleteScope::App("kitty".into())),
        ClientReq::ChangePassword {
            old: secret(PASSWORD),
            new: secret("another one"),
        },
    ];
    for req in refused {
        assert_eq!(client.call(req).await, locked());
    }
    for req in [
        ClientReq::Touch,
        ClientReq::Lock,
        ClientReq::Pause,
        ClientReq::Resume,
    ] {
        assert_eq!(client.call(req).await, ClientResp::Ok);
    }
    assert_eq!(client.status().await.vault, VaultState::Locked);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_connection_cannot_search_after_unlock() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let mut intruder = daemon.client().await;

    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    assert!(matches!(ui.search("x").await, ClientResp::Hits(_)));
    assert_eq!(intruder.search("x").await, locked());
    assert_eq!(
        intruder.call(ClientReq::Timeline { from: 0, to: 1 }).await,
        locked()
    );

    drop(ui);
    tokio::time::sleep(Duration::from_millis(300)).await;
    intruder
        .wait_for_vault(|vault| matches!(vault, VaultState::Unlocked { .. }))
        .await;
    assert_eq!(intruder.search("x").await, locked());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_whether_this_connection_holds_a_session() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let mut cli = daemon.client().await;
    assert!(!ui.status().await.session);

    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    assert!(ui.status().await.session);
    let other = cli.status().await;
    assert!(matches!(other.vault, VaultState::Unlocked { .. }));
    assert!(!other.session);

    assert_eq!(cli.call(ClientReq::Lock).await, ClientResp::Ok);
    assert_eq!(cli.unlock(PASSWORD).await, ClientResp::Ok);
    let after = ui.status().await;
    assert!(matches!(after.vault, VaultState::Unlocked { .. }));
    assert!(!after.session, "a lock in between ended the UI's session");
    assert!(cli.status().await.session);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_delays_and_errors() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let unicode = "pässwörd 日本語 🔑";
    let (daemon, mut client) = initialized(test_config(root.path()), &ocr).await;
    assert_eq!(client.call(ClientReq::Lock).await, ClientResp::Ok);
    let mut fresh = daemon.client().await;
    let changed = fresh
        .call(ClientReq::ChangePassword {
            old: secret(PASSWORD),
            new: secret(unicode),
        })
        .await;
    assert_eq!(changed, locked());

    assert_eq!(client.unlock(PASSWORD).await, ClientResp::Ok);
    let changed = client
        .call(ClientReq::ChangePassword {
            old: secret(PASSWORD),
            new: secret(unicode),
        })
        .await;
    assert_eq!(changed, ClientResp::Ok);
    assert_eq!(client.call(ClientReq::Lock).await, ClientResp::Ok);

    for attempt in [PASSWORD, "nope", PASSWORD] {
        let started = Instant::now();
        assert_eq!(
            client.unlock(attempt).await,
            ClientResp::Err("bad password".into())
        );
        assert!(started.elapsed() >= Duration::from_secs(1));
    }
    let started = Instant::now();
    assert_eq!(
        client.unlock("").await,
        ClientResp::Err("password must not be empty".into())
    );
    assert!(started.elapsed() >= Duration::from_secs(1));
    let bad_code = client
        .call(ClientReq::UnlockRecovery {
            code: secret("0000-0000"),
        })
        .await;
    assert_eq!(bad_code, ClientResp::Err("bad password".into()));
    assert_eq!(
        client.unlock("nope").await,
        ClientResp::Err("bad password".into())
    );
    assert_eq!(client.status().await.vault, VaultState::Locked);

    let too_many = ClientResp::Err("too many attempts, try again later".into());
    let started = Instant::now();
    assert_eq!(fresh.unlock(unicode).await, too_many);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refusals are not delayed"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fresh.unlock(unicode).await, ClientResp::Ok);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_timeout_locks() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let cfg = Config {
        lock_timeout_secs: 1,
        ..test_config(root.path())
    };
    let (daemon, mut client) = initialized(cfg, &ocr).await;
    assert_eq!(client.unlock(PASSWORD).await, ClientResp::Ok);
    client
        .wait_for_vault(|vault| *vault == VaultState::Locked)
        .await;
    assert_eq!(client.search("x").await, locked());
    assert!(root.path().join("data/index.snap").exists());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_removes_last_minutes() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let mut capture = daemon.capture(&ocr).await;
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);

    capture.frame("kitty", "ephemeral note").await;
    ui.wait_for_hits("ephemeral", 1).await;
    capture.send(CaptureMsg::Forget { minutes: 5 }).await;
    ui.wait_for_hits("ephemeral", 0).await;

    capture.frame("kitty", "ghostly words").await;
    ui.wait_for_hits("ghostly", 1).await;
    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    let mut stranger = daemon.client().await;
    assert_eq!(
        stranger.call(ClientReq::Forget { minutes: 5 }).await,
        ClientResp::Ok
    );

    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    assert!(ui.hits("ghostly").await.is_empty());
    assert!(ui.hits("ephemeral").await.is_empty());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_app_removes_hits() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let mut capture = daemon.capture(&ocr).await;
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;

    capture.frame("firefox", "mango browser").await;
    capture.frame("kitty", "mango terminal").await;
    let hits = ui.wait_for_hits("mango", 2).await;
    let firefox = hits.iter().find(|hit| hit.app == "firefox").unwrap().r;

    let deleted = ui
        .call(ClientReq::Delete(DeleteScope::App("Firefox".into())))
        .await;
    assert_eq!(deleted, ClientResp::Ok);
    let left = ui.hits("mango").await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].app, "kitty");
    assert_eq!(
        ui.call(ClientReq::Image {
            r: firefox,
            thumb: true
        })
        .await,
        ClientResp::Err("not found".into())
    );

    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    assert_eq!(ui.hits("mango").await.len(), 1);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_range_removes_hits() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let mut capture = daemon.capture(&ocr).await;
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    capture.frame("kitty", "papaya ledger").await;
    ui.wait_for_hits("papaya", 1).await;

    let now = now_ms();
    let range = DeleteScope::Range {
        from: now - 60_000,
        to: now + 60_000,
    };
    assert_eq!(ui.call(ClientReq::Delete(range)).await, ClientResp::Ok);
    assert!(ui.hits("papaya").await.is_empty());
    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    assert!(ui.hits("papaya").await.is_empty());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_client_does_not_kill_daemon() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let daemon = start(test_config(root.path()), &ocr).await;
    let client_socket = daemon.runtime_dir.join("client.sock");
    let capture_socket = daemon.runtime_dir.join("capture.sock");

    let mut payloads: Vec<Vec<u8>> = vec![vec![0xff; 64], u32::MAX.to_be_bytes().to_vec()];
    let mut garbage_body = 16u32.to_be_bytes().to_vec();
    garbage_body.extend([0xff; 16]);
    payloads.push(garbage_body);
    for socket in [&client_socket, &capture_socket] {
        for payload in &payloads {
            let mut rogue = connect(socket).await;
            rogue.write_all(payload).await.unwrap();
            let mut buf = [0u8; 1];
            let read = tokio::time::timeout(WAIT_LIMIT, rogue.read(&mut buf))
                .await
                .expect("daemon kept a garbage connection open");
            assert!(matches!(read, Ok(0) | Err(_)), "daemon answered garbage");
        }
    }

    let mut oversized = (100u32 * 1024).to_be_bytes().to_vec();
    oversized.extend([0u8; 1024]);
    let mut rogue = connect(&client_socket).await;
    rogue.write_all(&oversized).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(WAIT_LIMIT, rogue.read(&mut buf))
        .await
        .expect("daemon waited for an oversized client frame");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "daemon answered an oversized frame"
    );

    let mut client = daemon.client().await;
    assert_eq!(client.status().await.vault, VaultState::Uninitialized);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_survives_restart() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let cfg = test_config(root.path());
    let (daemon, mut ui) = initialized(cfg.clone(), &ocr).await;
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    let mut capture = daemon.capture(&ocr).await;
    capture.frame("kitty", "persistent walrus").await;
    ui.wait_for_hits("walrus", 1).await;
    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    drop(capture);
    drop(ui);
    daemon.stop().await;

    let ocr_calls = ocr.calls.load(Ordering::SeqCst);
    let daemon = start(cfg, &ocr).await;
    let mut ui = daemon.client().await;
    assert_eq!(ui.status().await.vault, VaultState::Locked);
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    assert_eq!(
        ui.hits("walrus").await.len(),
        1,
        "hit served from the snapshot"
    );
    ui.wait_until_indexed().await;
    assert_eq!(
        ui.hits("walrus").await.len(),
        1,
        "no duplicate after catch-up"
    );
    assert_eq!(ocr.calls.load(Ordering::SeqCst), ocr_calls);
    daemon.stop().await;
}

/// Forwards each cause and reports when the daemon said the lock was done.
struct ChannelSignals {
    causes: mpsc::Receiver<LockCause>,
    done: mpsc::Sender<LockCause>,
}

impl LockSignals for ChannelSignals {
    async fn watch(mut self, tx: mpsc::Sender<LockRequest>) {
        while let Some(cause) = self.causes.recv().await {
            let (request, locked) = LockRequest::new(cause);
            if tx.send(request).await.is_err() {
                break;
            }
            if locked.await.is_ok() {
                let _ = self.done.send(cause).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleep_signal_locks() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (signal, causes) = mpsc::channel(4);
    let (done, mut locked) = mpsc::channel(4);
    let daemon = start_with(test_config(root.path()), &ocr, move |cfg, ocr, stop| {
        tokio::spawn(capricornd::run_with_lock_signals(
            cfg,
            ocr,
            ChannelSignals { causes, done },
            stop,
        ))
    })
    .await;
    let mut ui = daemon.client().await;
    ui.init(PASSWORD).await;
    for cause in [LockCause::Sleep, LockCause::ScreenLock] {
        assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
        signal.send(cause).await.unwrap();
        let reported = tokio::time::timeout(WAIT_LIMIT, locked.recv()).await;
        assert_eq!(reported.unwrap(), Some(cause));
        assert_eq!(
            ui.status().await.vault,
            VaultState::Locked,
            "done came before the lock"
        );
        assert!(root.path().join("data/index.snap").exists());
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_reaches_capture_client() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    let mut capture = daemon.capture(&ocr).await;
    capture
        .send(CaptureMsg::Recorder(RecorderState::Recording))
        .await;
    ui.wait_for_status(|status| status.recorder == Some(RecorderState::Recording))
        .await;

    assert_eq!(ui.call(ClientReq::Pause).await, ClientResp::Ok);
    let ctl: CaptureCtl = read_msg(&mut capture.stream).await.unwrap();
    assert_eq!(ctl, CaptureCtl::Pause);
    assert_eq!(ui.call(ClientReq::Resume).await, ClientResp::Ok);
    let ctl: CaptureCtl = read_msg(&mut capture.stream).await.unwrap();
    assert_eq!(ctl, CaptureCtl::Resume);
    for expected in [CaptureCtl::Pause, CaptureCtl::Resume] {
        assert_eq!(ui.call(ClientReq::TogglePause).await, ClientResp::Ok);
        let ctl: CaptureCtl = read_msg(&mut capture.stream).await.unwrap();
        assert_eq!(ctl, expected);
    }

    let mut newer = daemon.capture(&ocr).await;
    newer
        .send(CaptureMsg::Recorder(RecorderState::Paused))
        .await;
    ui.wait_for_status(|status| status.recorder == Some(RecorderState::Paused))
        .await;
    capture
        .send(CaptureMsg::Recorder(RecorderState::Idle))
        .await;
    drop(capture);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        ui.status().await.recorder,
        Some(RecorderState::Paused),
        "an older connection does not override the newest one"
    );
    drop(newer);
    ui.wait_for_status(|status| status.recorder.is_none()).await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forgotten_captures_stay_gone_after_restart() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let cfg = test_config(root.path());
    let (daemon, mut ui) = initialized(cfg.clone(), &ocr).await;
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    let mut capture = daemon.capture(&ocr).await;
    capture.frame("kitty", "regrettable tangerine").await;
    ui.wait_for_hits("tangerine", 1).await;
    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    drop(capture);
    drop(ui);
    daemon.stop().await;

    let daemon = start(cfg, &ocr).await;
    let mut ui = daemon.client().await;
    assert_eq!(
        ui.call(ClientReq::Forget { minutes: 5 }).await,
        ClientResp::Ok
    );
    let mut capture = daemon.capture(&ocr).await;
    let calls = ocr.calls.load(Ordering::SeqCst);
    capture.frame("kitty", "harmless cucumber").await;
    wait_until_stored(&root.path().join("data"), &ocr, calls + 1).await;

    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    assert!(ui.hits("tangerine").await.is_empty());
    let hits = ui.hits("cucumber").await;
    assert_eq!(hits.len(), 1);
    match ui.call(ClientReq::Detail(hits[0].r)).await {
        ClientResp::Detail(detail) => assert!(detail.text.contains("cucumber")),
        other => panic!("expected detail, got {other:?}"),
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_removes_captures_with_old_timestamps() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let (daemon, mut ui) = initialized(test_config(root.path()), &ocr).await;
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    let mut capture = daemon.capture(&ocr).await;
    capture
        .frame_at("kitty", "backdated pomegranate", now_ms() - 30 * 60_000)
        .await;
    ui.wait_for_hits("pomegranate", 1).await;
    capture.send(CaptureMsg::Forget { minutes: 5 }).await;
    ui.wait_for_hits("pomegranate", 0).await;
    assert_eq!(ui.call(ClientReq::Lock).await, ClientResp::Ok);
    assert_eq!(ui.unlock(PASSWORD).await, ClientResp::Ok);
    ui.wait_until_indexed().await;
    assert!(ui.hits("pomegranate").await.is_empty());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_does_not_wait_for_a_slow_unlock() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let cfg = Config {
        kdf: Some(KdfOverride {
            memory_kib: 128 * 1024,
            iterations: 8,
            parallelism: 1,
        }),
        ..test_config(root.path())
    };
    let (daemon, _) = initialized(cfg, &ocr).await;
    let mut slow = daemon.client().await;
    let unlocking = tokio::spawn(async move { slow.unlock(PASSWORD).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut other = daemon.client().await;
    assert_eq!(
        other.unlock(PASSWORD).await,
        ClientResp::Err("busy, try again".into())
    );
    let mut locker = daemon.client().await;
    assert_eq!(locker.call(ClientReq::Lock).await, ClientResp::Ok);
    assert!(!unlocking.is_finished(), "lock waited for the unlock");
    assert_eq!(
        unlocking.await.unwrap(),
        ClientResp::Err("locked during unlock, try again".into())
    );
    assert_eq!(locker.status().await.vault, VaultState::Locked);
    assert_eq!(locker.unlock(PASSWORD).await, ClientResp::Ok);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_per_socket_are_capped() {
    let root = TempDir::new().unwrap();
    let ocr = Arc::new(FakeOcr::default());
    let daemon = start(test_config(root.path()), &ocr).await;
    let mut held = Vec::new();
    for _ in 0..64 {
        let mut client = daemon.client().await;
        client.status().await;
        held.push(client);
    }
    let mut refused = connect(&daemon.runtime_dir.join("client.sock")).await;
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(WAIT_LIMIT, refused.read(&mut buf))
        .await
        .expect("daemon kept a connection over the cap");
    assert!(matches!(read, Ok(0) | Err(_)));

    held.pop();
    eventually("a free connection slot", || {
        let socket = daemon.runtime_dir.join("client.sock");
        async move {
            let mut stream = connect(&socket).await;
            write_msg(&mut stream, &ClientReq::Status).await.unwrap();
            read_msg::<_, ClientResp>(&mut stream).await.is_ok()
        }
    })
    .await;
    drop(held);
    daemon.stop().await;
}

#[tokio::test]
async fn refuses_to_start_without_allowed_uids() {
    let root = TempDir::new().unwrap();
    let ocr: Arc<dyn Ocr> = Arc::new(FakeOcr::default());
    let cfg = Config {
        allow_any_uid: false,
        ..test_config(root.path())
    };
    let error = capricornd::run_with(cfg, ocr, std::future::pending())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("allowed_uids"), "{error:#}");
}
