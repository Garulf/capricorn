use std::path::PathBuf;

use anyhow::Context;
use capricorn_core::framing::write_msg;
use capricorn_core::paths::capture_socket;
use capricorn_core::proto::{CaptureMsg, ClipboardEntry, Frame, Ts, WindowContext};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Send one fake capture to the daemon and exit. Needs neither Wayland nor Hyprland.
#[derive(Debug, clap::Args)]
#[command(group = clap::ArgGroup::new("payload").required(true).args(["image", "clipboard"]))]
pub struct InjectArgs {
    /// PNG to send as a screen frame.
    #[arg(long)]
    pub image: Option<PathBuf>,
    /// Text to send as a clipboard entry instead of a frame.
    #[arg(long)]
    pub clipboard: Option<String>,
    #[arg(long)]
    pub class: String,
    #[arg(long)]
    pub title: String,
    #[arg(long, default_value = "1")]
    pub workspace: String,
    #[arg(long, default_value = "INJECT-1")]
    pub output: String,
}

pub async fn run(args: InjectArgs) -> anyhow::Result<()> {
    let msg = message(&args, crate::recorder::now_ms())?;
    let socket = capture_socket();
    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    write_msg(&mut stream, &msg).await?;
    stream.shutdown().await?;
    Ok(())
}

fn message(args: &InjectArgs, ts: Ts) -> anyhow::Result<CaptureMsg> {
    let window = WindowContext {
        class: args.class.clone(),
        title: args.title.clone(),
        workspace: args.workspace.clone(),
        output: args.output.clone(),
        pid: None,
        cwd: None,
    };
    if let Some(text) = &args.clipboard {
        return Ok(CaptureMsg::Clipboard(ClipboardEntry {
            ts,
            text: text.clone(),
            source: window,
        }));
    }
    let path = args
        .image
        .as_ref()
        .context("--image or --clipboard is required")?;
    let img = image::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .into_rgba8();
    Ok(CaptureMsg::Frame(Frame {
        ts,
        output: args.output.clone(),
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
        window,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn args() -> InjectArgs {
        InjectArgs {
            image: None,
            clipboard: None,
            class: "kitty".into(),
            title: "notes".into(),
            workspace: "3".into(),
            output: "DP-9".into(),
        }
    }

    #[test]
    fn png_becomes_frame_with_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        let mut img = RgbaImage::from_pixel(3, 2, Rgba([10, 20, 30, 255]));
        img.put_pixel(2, 1, Rgba([1, 2, 3, 255]));
        img.save(&path).unwrap();

        let msg = message(
            &InjectArgs {
                image: Some(path),
                ..args()
            },
            77,
        )
        .unwrap();
        let CaptureMsg::Frame(frame) = msg else {
            panic!("expected a frame, got {msg:?}");
        };
        assert_eq!((frame.ts, frame.width, frame.height), (77, 3, 2));
        assert_eq!(frame.output, "DP-9");
        assert_eq!(frame.rgba.len(), 3 * 2 * 4);
        assert_eq!(&frame.rgba[20..24], &[1, 2, 3, 255]);
        assert_eq!(frame.window.class, "kitty");
        assert_eq!(frame.window.workspace, "3");
    }

    #[test]
    fn clipboard_text_becomes_entry() {
        let msg = message(
            &InjectArgs {
                clipboard: Some("hello".into()),
                ..args()
            },
            5,
        )
        .unwrap();
        let CaptureMsg::Clipboard(entry) = msg else {
            panic!("expected a clipboard entry, got {msg:?}");
        };
        assert_eq!(entry.text, "hello");
        assert_eq!(entry.source.title, "notes");
    }

    #[test]
    fn unreadable_image_is_error() {
        let missing = InjectArgs {
            image: Some("/nonexistent/shot.png".into()),
            ..args()
        };
        assert!(message(&missing, 0).is_err());
    }
}
