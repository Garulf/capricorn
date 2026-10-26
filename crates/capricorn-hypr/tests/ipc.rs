use capricorn_hypr::{Client, Error, Event, Hypr, Monitor};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

const SIG: &str = "abc123_1700000000_42";

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn bind(runtime: &Path, socket: &str) -> UnixListener {
    let dir = runtime.join("hypr").join(SIG);
    std::fs::create_dir_all(&dir).unwrap();
    UnixListener::bind(dir.join(socket)).unwrap()
}

/// Serves one request: returns what the client wrote after replying.
fn serve_once(listener: UnixListener, reply: String) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let read = conn.read(&mut buf).await.unwrap();
        conn.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..read]).into_owned()
    })
}

fn hypr(runtime: &tempfile::TempDir) -> Hypr {
    Hypr::new(PathBuf::from(runtime.path()), SIG)
}

#[tokio::test]
async fn query_clients_sends_json_flag_and_parses_reply() {
    let runtime = tempfile::tempdir().unwrap();
    let server = serve_once(
        bind(runtime.path(), ".socket.sock"),
        fixture("clients.json"),
    );

    let clients: Vec<Client> = hypr(&runtime).query("j/clients").await.unwrap();

    assert_eq!(server.await.unwrap(), "j/clients");
    assert_eq!(clients.len(), 3);
    assert_eq!(clients[1].class, "firefox");
}

#[tokio::test]
async fn monitors_and_active_window_helpers() {
    let runtime = tempfile::tempdir().unwrap();
    let listener = bind(runtime.path(), ".socket.sock");
    let hypr = hypr(&runtime);

    let server = serve_once(listener, fixture("monitors.json"));
    let monitors: Vec<Monitor> = hypr.monitors().await.unwrap();
    assert_eq!(server.await.unwrap(), "j/monitors");
    assert_eq!(monitors.len(), 2);
}

#[tokio::test]
async fn active_window_is_none_for_empty_object() {
    let runtime = tempfile::tempdir().unwrap();
    let server = serve_once(
        bind(runtime.path(), ".socket.sock"),
        fixture("activewindow_none.json"),
    );
    assert_eq!(hypr(&runtime).active_window().await.unwrap(), None);
    assert_eq!(server.await.unwrap(), "j/activewindow");
}

#[tokio::test]
async fn dispatch_ok_and_rejected() {
    let runtime = tempfile::tempdir().unwrap();
    let listener = bind(runtime.path(), ".socket.sock");
    let hypr = hypr(&runtime);

    let server = serve_once(listener, "ok".into());
    hypr.dispatch("dispatch exec [workspace 3] kitty")
        .await
        .unwrap();
    assert_eq!(server.await.unwrap(), "dispatch exec [workspace 3] kitty");
}

#[tokio::test]
async fn dispatch_error_reply_is_rejected() {
    let runtime = tempfile::tempdir().unwrap();
    let server = serve_once(
        bind(runtime.path(), ".socket.sock"),
        "invalid dispatcher".into(),
    );
    let err = hypr(&runtime).dispatch("dispatch nope").await.unwrap_err();
    assert!(matches!(err, Error::Rejected(ref msg) if msg == "invalid dispatcher"));
    server.await.unwrap();
}

#[tokio::test]
async fn query_with_garbage_reply_is_a_json_error() {
    let runtime = tempfile::tempdir().unwrap();
    let server = serve_once(bind(runtime.path(), ".socket.sock"), "not json".into());
    let result: Result<Vec<Client>, _> = hypr(&runtime).query("j/clients").await;
    assert!(matches!(result, Err(Error::Json(_))));
    server.await.unwrap();
}

#[tokio::test]
async fn missing_socket_is_an_io_error() {
    let runtime = tempfile::tempdir().unwrap();
    let result: Result<Vec<Client>, _> = hypr(&runtime).query("j/clients").await;
    assert!(matches!(result, Err(Error::Io(_))));
}

#[tokio::test]
async fn events_stream_parses_lines_until_close() {
    let runtime = tempfile::tempdir().unwrap();
    let listener = bind(runtime.path(), ".socket2.sock");
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        conn.write_all(
            b"workspace>>3\nactivewindow>>firefox,a, b >> c\nmonitoradded>>DP-2\nfullscreen>>1\n",
        )
        .await
        .unwrap();
    });

    let events: Vec<Event> = hypr(&runtime).events().await.unwrap().collect().await;

    server.await.unwrap();
    assert_eq!(
        events,
        [
            Event::Workspace("3".into()),
            Event::ActiveWindow {
                class: "firefox".into(),
                title: "a, b >> c".into()
            },
            Event::Other,
            Event::Fullscreen(true),
        ]
    );
}

fn serve_replies(
    listener: UnixListener,
    replies: Vec<&'static str>,
) -> tokio::task::JoinHandle<Vec<String>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let read = conn.read(&mut buf).await.unwrap();
            conn.write_all(reply.as_bytes()).await.unwrap();
            requests.push(String::from_utf8_lossy(&buf[..read]).into_owned());
        }
        requests
    })
}

fn kitty_plan() -> capricorn_hypr::ReopenPlan {
    capricorn_hypr::ReopenPlan {
        lua: r#"hl.dsp.exec_cmd("kitty", { workspace = "3" })"#.into(),
        legacy: "[workspace 3] kitty".into(),
    }
}

#[tokio::test]
async fn reopen_uses_lua_dispatch_when_accepted() {
    let runtime = tempfile::tempdir().unwrap();
    let server = serve_replies(bind(runtime.path(), ".socket.sock"), vec!["ok"]);
    hypr(&runtime).reopen(&kitty_plan()).await.unwrap();
    assert_eq!(
        server.await.unwrap(),
        vec![r#"dispatch hl.dsp.exec_cmd("kitty", { workspace = "3" })"#]
    );
}

#[tokio::test]
async fn reopen_falls_back_to_hyprlang_exec() {
    let runtime = tempfile::tempdir().unwrap();
    let server = serve_replies(
        bind(runtime.path(), ".socket.sock"),
        vec!["Invalid dispatcher", "ok"],
    );
    hypr(&runtime).reopen(&kitty_plan()).await.unwrap();
    assert_eq!(
        server.await.unwrap()[1],
        "dispatch exec [workspace 3] kitty"
    );
}
