use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

const MAX_BYTES_PER_CONNECTION: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCmd {
    Search,
    Timeline,
    Hide,
}

pub fn parse_command(line: &str) -> Option<ControlCmd> {
    match line.trim() {
        "search" => Some(ControlCmd::Search),
        "timeline" => Some(ControlCmd::Timeline),
        "hide" => Some(ControlCmd::Hide),
        _ => None,
    }
}

pub fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|dir| PathBuf::from(dir).join("capricorn-ui.sock"))
}

/// Refuses to start next to a live instance; a leftover socket file is replaced.
pub fn bind(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("capricorn-ui already running on {}", path.display()),
        ));
    }
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub async fn serve(listener: UnixListener, commands: mpsc::Sender<ControlCmd>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(read_commands(stream, commands.clone()));
    }
}

async fn read_commands(stream: UnixStream, commands: mpsc::Sender<ControlCmd>) {
    let mut lines = BufReader::new(stream.take(MAX_BYTES_PER_CONNECTION)).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_command(&line) {
            Some(cmd) => {
                if commands.send(cmd).await.is_err() {
                    return;
                }
            }
            None => tracing::warn!("ignoring unknown control command"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn parses_known_commands() {
        assert_eq!(parse_command("search\n"), Some(ControlCmd::Search));
        assert_eq!(parse_command(" timeline "), Some(ControlCmd::Timeline));
        assert_eq!(parse_command("hide"), Some(ControlCmd::Hide));
        assert_eq!(parse_command("rm -rf"), None);
    }

    #[tokio::test]
    async fn socket_delivers_commands_and_rejects_second_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.sock");
        let listener = UnixListener::from_std(bind(&path).unwrap()).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        tokio::spawn(serve(listener, tx));

        let mut client = UnixStream::connect(&path).await.unwrap();
        client.write_all(b"search\nbogus\nhide\n").await.unwrap();
        assert_eq!(rx.recv().await, Some(ControlCmd::Search));
        assert_eq!(rx.recv().await, Some(ControlCmd::Hide));
        assert!(bind(&path).is_err());
    }
}
