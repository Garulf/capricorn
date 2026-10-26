use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};

use capricorn_core::Secret;
use capricorn_core::framing::{read_msg, write_msg};
use capricorn_core::proto::{
    ClientReq, ClientResp, Hit, Kind, RecordDetail, RecordRef, RecorderState, Status, VaultState,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

type Handler = dyn Fn(&ClientReq) -> ClientResp + Send + Sync;

struct Env {
    runtime: tempfile::TempDir,
    xdg: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ClientReq>>>,
}

impl Env {
    fn new() -> Self {
        Self {
            runtime: tempfile::tempdir().unwrap(),
            xdg: tempfile::tempdir().unwrap(),
            requests: Arc::default(),
        }
    }

    fn start_daemon(&self, handler: impl Fn(&ClientReq) -> ClientResp + Send + Sync + 'static) {
        let listener = UnixListener::bind(self.runtime.path().join("client.sock")).unwrap();
        let handler: Arc<Handler> = Arc::new(handler);
        let requests = self.requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let handler = handler.clone();
                let requests = requests.clone();
                tokio::spawn(async move {
                    while let Ok(req) = read_msg::<_, ClientReq>(&mut conn).await {
                        let resp = handler(&req);
                        requests.lock().unwrap().push(req);
                        if write_msg(&mut conn, &resp).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_capricorn"));
        cmd.args(args)
            .env_clear()
            .env("CAPRICORN_RUNTIME_DIR", self.runtime.path())
            .env("XDG_RUNTIME_DIR", self.xdg.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    async fn run(&self, args: &[&str], stdin: &str) -> Output {
        run_with_stdin(self.command(args), stdin).await
    }

    fn requests(&self) -> Vec<ClientReq> {
        self.requests.lock().unwrap().clone()
    }
}

async fn run_with_stdin(mut cmd: Command, stdin: &str) -> Output {
    let mut child = cmd.spawn().unwrap();
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin.as_bytes()).await.unwrap();
    drop(pipe);
    child.wait_with_output().await.unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn status(vault: VaultState) -> ClientResp {
    ClientResp::Status(Status {
        vault,
        recorder: Some(RecorderState::Recording),
        pending_index: 0,
        session: false,
    })
}

fn hit(hour: u32, seq: u32, app: &str) -> Hit {
    Hit {
        r: RecordRef { hour, seq },
        ts: 1_700_000_000_000,
        kind: Kind::Screen,
        app: app.into(),
        title: "notes".into(),
        workspace: "3".into(),
        context: None,
        snippet: "the quarterly report".into(),
        highlights: vec![(4, 13)],
    }
}

fn accepting_daemon(
    password: &'static str,
    on_other: impl Fn(&ClientReq) -> ClientResp + Send + Sync + 'static,
) -> impl Fn(&ClientReq) -> ClientResp + Send + Sync + 'static {
    move |req| match req {
        ClientReq::Status => status(VaultState::Locked),
        ClientReq::Unlock { password: given } if given.expose() == password => ClientResp::Ok,
        ClientReq::Unlock { .. } => ClientResp::Err("wrong password".into()),
        other => on_other(other),
    }
}

#[tokio::test]
async fn search_json_output() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("correct horse", |req| match req {
        ClientReq::Search(_) => ClientResp::Hits(vec![hit(491_000, 7, "firefox")]),
        _ => ClientResp::Err("unexpected".into()),
    }));

    let out = env
        .run(
            &[
                "search",
                "quarterly",
                "report",
                "--json",
                "--app",
                "firefox",
                "--limit",
                "5",
                "--password-stdin",
            ],
            "correct horse\n",
        )
        .await;

    assert!(out.status.success(), "{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json[0]["ref"], "491000:7");
    assert_eq!(json[0]["app"], "firefox");
    assert_eq!(json[0]["kind"], "screen");
    assert_eq!(json[0]["snippet"], "the quarterly report");

    let requests = env.requests();
    assert!(matches!(requests[0], ClientReq::Status));
    assert!(matches!(requests[1], ClientReq::Unlock { .. }));
    let ClientReq::Search(query) = &requests[2] else {
        panic!("expected search, got {:?}", requests[2]);
    };
    assert_eq!(query.text, "quarterly report");
    assert_eq!(query.app.as_deref(), Some("firefox"));
    assert_eq!(query.limit, 5);
}

#[tokio::test]
async fn search_plain_output_lists_ref_app_and_snippet() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("pw", |_| {
        ClientResp::Hits(vec![hit(491_000, 7, "firefox")])
    }));

    let out = env
        .run(&["search", "report", "--password-stdin"], "pw\n")
        .await;

    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("491000:7") && text.contains("firefox"),
        "{text}"
    );
    assert!(text.contains("the quarterly report"), "{text}");
    assert!(!text.contains('\u{1b}'), "no colour codes when piped");
}

#[tokio::test]
async fn search_on_uninitialized_vault_fails_without_asking_for_password() {
    let env = Env::new();
    env.start_daemon(|_| status(VaultState::Uninitialized));

    let out = env.run(&["search", "x", "--password-stdin"], "pw\n").await;

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("capricorn init"), "{}", stderr(&out));
    assert_eq!(env.requests().len(), 1);
}

#[tokio::test]
async fn unlock_wrong_password_exits_nonzero() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("right", |_| ClientResp::Ok));

    let out = env.run(&["unlock", "--password-stdin"], "wrong\n").await;

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("wrong password"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[tokio::test]
async fn unlock_right_password_succeeds() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("right", |_| ClientResp::Ok));

    let out = env.run(&["unlock", "--password-stdin"], "right\n").await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[tokio::test]
async fn empty_stdin_is_an_error_not_an_empty_password() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("right", |_| ClientResp::Ok));

    let out = env.run(&["unlock", "--password-stdin"], "").await;

    assert_eq!(out.status.code(), Some(1));
    assert!(
        env.requests()
            .iter()
            .all(|r| !matches!(r, ClientReq::Unlock { .. }))
    );
}

#[tokio::test]
async fn unreachable_daemon_exits_3() {
    let env = Env::new();

    let out = env.run(&["lock"], "").await;

    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("capricornd"), "{}", stderr(&out));
}

#[tokio::test]
async fn waybar_status_prints_off_when_daemon_is_down() {
    let env = Env::new();

    let out = env.run(&["status", "--waybar"], "").await;

    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json["alt"], "off");
}

#[tokio::test]
async fn waybar_status_reports_recording() {
    let env = Env::new();
    env.start_daemon(|_| status(VaultState::Unlocked { locks_in_secs: 600 }));

    let out = env.run(&["status", "--waybar"], "").await;

    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json["alt"], "recording");
    assert_eq!(json["class"], "recording");
}

#[tokio::test]
async fn waybar_status_reports_recording_while_the_vault_is_locked() {
    let env = Env::new();
    env.start_daemon(|_| status(VaultState::Locked));

    let out = env.run(&["status", "--waybar"], "").await;

    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json["class"], "recording");
    assert_eq!(json["tooltip"], "recording, vault locked");
}

#[tokio::test]
async fn plain_status_exits_3_when_daemon_is_down() {
    let env = Env::new();
    let out = env.run(&["status"], "").await;
    assert_eq!(out.status.code(), Some(3));
}

#[tokio::test]
async fn status_follow_prints_first_state_then_waits() {
    let env = Env::new();
    env.start_daemon(|_| status(VaultState::Locked));
    let mut child = env
        .command(&["status", "--waybar", "--follow"])
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next_line().await.unwrap().unwrap();

    let json: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(json["alt"], "locked");
    child.kill().await.unwrap();
}

#[tokio::test]
async fn show_writes_ui_control_line() {
    let env = Env::new();
    let listener = UnixListener::bind(env.xdg.path().join("capricorn-ui.sock")).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut received = String::new();
        conn.read_to_string(&mut received).await.unwrap();
        received
    });

    let out = env.run(&["show", "search"], "").await;

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(server.await.unwrap(), "search\n");
}

#[tokio::test]
async fn show_timeline_writes_timeline() {
    let env = Env::new();
    let listener = UnixListener::bind(env.xdg.path().join("capricorn-ui.sock")).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut received = String::new();
        conn.read_to_string(&mut received).await.unwrap();
        received
    });

    env.run(&["show", "timeline"], "").await;

    assert_eq!(server.await.unwrap(), "timeline\n");
}

#[tokio::test]
async fn show_without_ui_fails_with_hint() {
    let env = Env::new();
    let out = env.run(&["show", "search"], "").await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("capricorn-ui"), "{}", stderr(&out));
}

#[tokio::test]
async fn init_prints_recovery_code() {
    let env = Env::new();
    env.start_daemon(|req| match req {
        ClientReq::Init { recovery: true, .. } => {
            ClientResp::RecoveryCode(Secret::new("AAAA-BBBB-CCCC".into()))
        }
        _ => ClientResp::Err("unexpected".into()),
    });

    let out = env.run(&["init", "--password-stdin"], "pw\n").await;

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("AAAA-BBBB-CCCC"));
    assert!(stderr(&out).contains("offline"), "{}", stderr(&out));
}

#[tokio::test]
async fn init_no_recovery_asks_daemon_for_none() {
    let env = Env::new();
    env.start_daemon(|_| ClientResp::Ok);

    let out = env
        .run(&["init", "--no-recovery", "--password-stdin"], "pw\n")
        .await;

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(matches!(
        env.requests()[0],
        ClientReq::Init {
            recovery: false,
            ..
        }
    ));
    assert!(stdout(&out).is_empty());
}

#[tokio::test]
async fn init_rejects_empty_password() {
    let env = Env::new();
    env.start_daemon(|_| ClientResp::Ok);
    let out = env.run(&["init", "--password-stdin"], "\n").await;
    assert_eq!(out.status.code(), Some(1));
    assert!(env.requests().is_empty());
}

#[tokio::test]
async fn forget_defaults_to_five_minutes() {
    let env = Env::new();
    env.start_daemon(|_| ClientResp::Ok);

    let out = env.run(&["forget"], "").await;

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.requests(), vec![ClientReq::Forget { minutes: 5 }]);
}

#[tokio::test]
async fn pause_resume_lock_send_matching_requests() {
    let env = Env::new();
    env.start_daemon(|_| ClientResp::Ok);
    for sub in ["pause", "resume", "lock"] {
        assert!(env.run(&[sub], "").await.status.success(), "{sub}");
    }
    assert_eq!(
        env.requests(),
        vec![ClientReq::Pause, ClientReq::Resume, ClientReq::Lock]
    );
}

#[tokio::test]
async fn passwd_sends_old_then_new_from_stdin() {
    let env = Env::new();
    env.start_daemon(|_| ClientResp::Ok);

    let out = env
        .run(&["passwd", "--password-stdin"], "old-pw\nnew-pw\n")
        .await;

    assert!(out.status.success(), "{}", stderr(&out));
    let ClientReq::ChangePassword { old, new } = &env.requests()[0] else {
        panic!("expected ChangePassword");
    };
    assert_eq!((old.expose(), new.expose()), ("old-pw", "new-pw"));
}

fn detail(app: &str, cwd: Option<&str>) -> ClientResp {
    ClientResp::Detail(RecordDetail {
        hit: hit(491_000, 7, app),
        text: String::new(),
        cwd: cwd.map(Into::into),
        url: None,
    })
}

async fn serve_hypr(xdg: &Path, reply: &'static str) -> tokio::task::JoinHandle<String> {
    let dir = xdg.join("hypr").join("sig_1_2");
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let read = conn.read(&mut buf).await.unwrap();
        conn.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..read]).into_owned()
    })
}

#[tokio::test]
async fn reopen_unlocks_fetches_detail_and_dispatches_to_hyprland() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("pw", |req| match req {
        ClientReq::Detail(_) => detail("kitty", Some("/home/u/src")),
        _ => ClientResp::Err("unexpected".into()),
    }));
    let hypr = serve_hypr(env.xdg.path(), "ok").await;
    let mut cmd = env.command(&["reopen", "491000:7", "--password-stdin"]);
    cmd.env("HYPRLAND_INSTANCE_SIGNATURE", "sig_1_2");

    let out = run_with_stdin(cmd, "pw\n").await;

    assert!(out.status.success(), "{}", stderr(&out));
    let dispatched = hypr.await.unwrap();
    assert!(
        dispatched.contains("kitty") && dispatched.contains("/home/u/src"),
        "{dispatched}"
    );
    assert!(dispatched.contains("workspace"), "{dispatched}");
    let requests = env.requests();
    assert_eq!(
        requests.last(),
        Some(&ClientReq::Detail(RecordRef {
            hour: 491_000,
            seq: 7
        }))
    );
}

#[tokio::test]
async fn reopen_unknown_app_without_desktop_file_fails() {
    let env = Env::new();
    env.start_daemon(accepting_daemon("pw", |_| detail("no-such-app-xyz", None)));
    let mut cmd = env.command(&["reopen", "1:2", "--password-stdin"]);
    cmd.env("HYPRLAND_INSTANCE_SIGNATURE", "sig_1_2")
        .env("XDG_DATA_DIRS", env.xdg.path());

    let out = run_with_stdin(cmd, "pw\n").await;

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no-such-app-xyz"), "{}", stderr(&out));
}

#[tokio::test]
async fn reopen_rejects_malformed_ref() {
    let env = Env::new();
    let out = env.run(&["reopen", "nonsense"], "").await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("hour:seq"), "{}", stderr(&out));
}
