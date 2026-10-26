use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use capricorn_core::proto::{
    ClientReq, ClientResp, Hit, RecordDetail, RecordRef, SearchQuery, Status, VaultState,
};
use capricorn_hypr::{Hypr, ReopenSpec, reopen_plan};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::cli::{Cli, Command, KindArg, PasswordArgs, View};
use crate::client::{DaemonClient, unexpected_reply};
use crate::error::{CliError, Result};
use crate::password::PasswordInput;
use crate::render::{hit_json, hit_text, status_text};
use crate::waybar::{waybar, waybar_unreachable};

const FOLLOW_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init {
            no_recovery,
            password,
        } => init(no_recovery, password).await,
        Command::Unlock { password } => unlock(password).await,
        Command::Lock => simple(ClientReq::Lock).await,
        Command::Status { waybar, follow } => status(waybar, follow).await,
        Command::Search {
            query,
            app,
            kind,
            json,
            limit,
            password,
        } => {
            let query = search_query(&query, app, kind, limit);
            search(query, json, password).await
        }
        Command::Forget { minutes } => simple(ClientReq::Forget { minutes }).await,
        Command::Pause => simple(ClientReq::Pause).await,
        Command::Resume => simple(ClientReq::Resume).await,
        Command::Passwd { password } => passwd(password).await,
        Command::Show { view } => show(view).await,
        Command::Reopen { record, password } => reopen(record, password).await,
    }
}

async fn simple(req: ClientReq) -> Result<()> {
    DaemonClient::connect().await?.call_ok(&req).await
}

async fn init(no_recovery: bool, args: PasswordArgs) -> Result<()> {
    let password = PasswordInput::new(args.password_stdin).ask_new("New password: ")?;
    if password.expose().is_empty() {
        return Err(CliError::failed("password must not be empty"));
    }
    let mut client = DaemonClient::connect().await?;
    let req = ClientReq::Init {
        password,
        recovery: !no_recovery,
    };
    match client.call(&req).await? {
        ClientResp::RecoveryCode(code) => {
            eprintln!(
                "Recovery code: store it offline (paper, safe). It unlocks the vault if you forget the password and is not shown again."
            );
            println!("{}", code.expose());
            Ok(())
        }
        ClientResp::Ok => Ok(()),
        _ => Err(unexpected_reply()),
    }
}

/// Sessions are per connection, so every command that reads data unlocks its own.
async fn connect_unlocked(args: PasswordArgs) -> Result<DaemonClient> {
    let mut client = DaemonClient::connect().await?;
    if client.status().await?.vault == VaultState::Uninitialized {
        return Err(CliError::failed("no vault yet; run `capricorn init`"));
    }
    let password = PasswordInput::new(args.password_stdin).ask("Password: ")?;
    client.unlock(password).await?;
    Ok(client)
}

async fn unlock(args: PasswordArgs) -> Result<()> {
    connect_unlocked(args).await.map(drop)
}

async fn passwd(args: PasswordArgs) -> Result<()> {
    let input = PasswordInput::new(args.password_stdin);
    let old = input.ask("Current password: ")?;
    let new = input.ask_new("New password: ")?;
    if new.expose().is_empty() {
        return Err(CliError::failed("password must not be empty"));
    }
    DaemonClient::connect()
        .await?
        .request_with_password(&ClientReq::ChangePassword { old, new })
        .await
}

async fn status(use_waybar: bool, follow: bool) -> Result<()> {
    if follow {
        return follow_status(use_waybar).await;
    }
    match fetch_status().await {
        Err(CliError::Unreachable { .. }) if use_waybar => print_line(&status_line(true, None)),
        Err(err) => Err(err),
        Ok(status) => print_line(&status_line(use_waybar, Some(&status))),
    }
}

async fn fetch_status() -> Result<Status> {
    DaemonClient::connect().await?.status().await
}

fn status_line(use_waybar: bool, status: Option<&Status>) -> String {
    match (use_waybar, status) {
        (true, Some(status)) => waybar(status).to_string(),
        (true, None) => waybar_unreachable().to_string(),
        (false, Some(status)) => status_text(status),
        (false, None) => "daemon: not running".to_owned(),
    }
}

async fn follow_status(use_waybar: bool) -> Result<()> {
    let mut last = None;
    loop {
        let line = status_line(use_waybar, fetch_status().await.ok().as_ref());
        if last.as_ref() != Some(&line) {
            print_line(&line)?;
            last = Some(line);
        }
        tokio::time::sleep(FOLLOW_INTERVAL).await;
    }
}

fn print_line(line: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

fn search_query(
    words: &[String],
    app: Option<String>,
    kind: Option<KindArg>,
    limit: u32,
) -> SearchQuery {
    SearchQuery {
        text: words.join(" "),
        kind: kind.map(Into::into),
        app,
        workspace: None,
        from: None,
        to: None,
        limit,
    }
}

async fn search(query: SearchQuery, json: bool, args: PasswordArgs) -> Result<()> {
    let mut client = connect_unlocked(args).await?;
    let ClientResp::Hits(hits) = client.call(&ClientReq::Search(query)).await? else {
        return Err(unexpected_reply());
    };
    if json {
        print_line(&hits_json(&hits))
    } else {
        print_hits(&hits)
    }
}

fn hits_json(hits: &[Hit]) -> String {
    serde_json::Value::Array(hits.iter().map(hit_json).collect()).to_string()
}

fn print_hits(hits: &[Hit]) -> Result<()> {
    if hits.is_empty() {
        return print_line("No matches.");
    }
    let color = std::io::stdout().is_terminal();
    hits.iter()
        .try_for_each(|hit| print_line(&hit_text(hit, color)))
}

async fn show(view: View) -> Result<()> {
    let socket = ui_socket()?;
    let mut stream = UnixStream::connect(&socket).await.map_err(|err| {
        CliError::failed(format!(
            "cannot reach capricorn-ui at {}: {err} (is the UI running?)",
            socket.display()
        ))
    })?;
    stream.write_all(view.control_line().as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn ui_socket() -> Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("capricorn-ui.sock"))
        .ok_or_else(|| CliError::failed("XDG_RUNTIME_DIR is not set"))
}

async fn reopen(record: RecordRef, args: PasswordArgs) -> Result<()> {
    let mut client = connect_unlocked(args).await?;
    let ClientResp::Detail(detail) = client.call(&ClientReq::Detail(record)).await? else {
        return Err(unexpected_reply());
    };
    let spec = reopen_spec(detail);
    let plan = reopen_plan(&spec, &desktop_dirs_from_env()).ok_or_else(|| {
        CliError::failed(format!(
            "cannot build a launch command for `{}`",
            spec.class
        ))
    })?;
    Hypr::from_env()?.reopen(&plan).await?;
    Ok(())
}

fn reopen_spec(detail: RecordDetail) -> ReopenSpec {
    ReopenSpec {
        class: detail.hit.app,
        workspace: detail.hit.workspace,
        cwd: detail.cwd,
        url: detail.url,
    }
}

fn desktop_dirs_from_env() -> Vec<PathBuf> {
    desktop_dirs(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("XDG_DATA_DIRS"),
        std::env::var_os("HOME"),
    )
}

/// XDG order: the user's data home first, then each system data dir.
pub fn desktop_dirs(
    data_home: Option<OsString>,
    data_dirs: Option<OsString>,
    home: Option<OsString>,
) -> Vec<PathBuf> {
    let non_empty = |value: Option<OsString>| value.filter(|v| !v.is_empty());
    let home_dir = non_empty(data_home)
        .map(PathBuf::from)
        .or_else(|| non_empty(home).map(|h| PathBuf::from(h).join(".local/share")));
    let system = non_empty(data_dirs).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home_dir
        .into_iter()
        .chain(std::env::split_paths(&system).filter(|dir| !dir.as_os_str().is_empty()))
        .map(|dir| dir.join("applications"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(data_home: Option<&str>, data_dirs: Option<&str>, home: Option<&str>) -> Vec<String> {
        desktop_dirs(
            data_home.map(Into::into),
            data_dirs.map(Into::into),
            home.map(Into::into),
        )
        .iter()
        .map(|dir| dir.display().to_string())
        .collect()
    }

    #[test]
    fn data_home_comes_before_data_dirs() {
        assert_eq!(
            dirs(Some("/h/share"), Some("/a:/b"), Some("/h")),
            [
                "/h/share/applications",
                "/a/applications",
                "/b/applications"
            ]
        );
    }

    #[test]
    fn defaults_follow_the_xdg_spec() {
        assert_eq!(
            dirs(None, None, Some("/home/u")),
            [
                "/home/u/.local/share/applications",
                "/usr/local/share/applications",
                "/usr/share/applications"
            ]
        );
    }

    #[test]
    fn nothing_is_invented_without_home() {
        assert_eq!(
            dirs(Some(""), Some("/a::/b"), None),
            ["/a/applications", "/b/applications"]
        );
    }

    #[test]
    fn query_words_are_joined_and_kind_mapped() {
        let query = search_query(
            &["a".into(), "b".into()],
            Some("x".into()),
            Some(KindArg::Clipboard),
            3,
        );
        assert_eq!(query.text, "a b");
        assert_eq!(query.kind, Some(capricorn_core::proto::Kind::Clipboard));
        assert_eq!((query.app.as_deref(), query.limit), (Some("x"), 3));
    }

    #[test]
    fn plain_status_line_when_daemon_is_down() {
        assert_eq!(status_line(false, None), "daemon: not running");
        assert!(status_line(true, None).contains("\"alt\":\"off\""));
    }
}
