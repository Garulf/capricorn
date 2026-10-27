use std::path::Path;
use std::process::Output;

use capricorn_core::framing::read_msg;
use capricorn_core::proto::CaptureMsg;
use image::{Rgba, RgbaImage};
use tokio::net::UnixListener;

async fn inject_and_receive(runtime_dir: &Path, args: &[&str]) -> (Output, CaptureMsg) {
    let listener = UnixListener::bind(runtime_dir.join("capture.sock")).unwrap();
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_capricorn-capture"))
        .arg("inject")
        .args(args)
        .env("CAPRICORN_RUNTIME_DIR", runtime_dir)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .output();
    let receive = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_msg::<_, CaptureMsg>(&mut stream).await.unwrap()
    };
    let (output, msg) = tokio::join!(child, receive);
    (output.unwrap(), msg)
}

#[tokio::test]
async fn inject_sends_png_as_frame() {
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("shot.png");
    let mut img = RgbaImage::from_pixel(4, 3, Rgba([200, 100, 50, 255]));
    img.put_pixel(0, 0, Rgba([1, 2, 3, 255]));
    img.save(&png).unwrap();

    let (output, msg) = inject_and_receive(
        dir.path(),
        &[
            "--image",
            png.to_str().unwrap(),
            "--class",
            "firefox",
            "--title",
            "Capricorn canary page",
            "--workspace",
            "2",
        ],
    )
    .await;

    assert!(output.status.success(), "{output:?}");
    let CaptureMsg::Frame(frame) = msg else {
        panic!("expected a frame, got {msg:?}");
    };
    assert_eq!((frame.width, frame.height), (4, 3));
    assert_eq!(frame.rgba, img.into_raw());
    assert_eq!(frame.window.class, "firefox");
    assert_eq!(frame.window.title, "Capricorn canary page");
    assert_eq!(frame.window.workspace, "2");
    assert!(frame.ts > 0);
}

#[tokio::test]
async fn inject_sends_clipboard_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (output, msg) = inject_and_receive(
        dir.path(),
        &[
            "--clipboard",
            "copied canary text",
            "--class",
            "kitty",
            "--title",
            "shell",
        ],
    )
    .await;

    assert!(output.status.success(), "{output:?}");
    let CaptureMsg::Clipboard(entry) = msg else {
        panic!("expected a clipboard entry, got {msg:?}");
    };
    assert_eq!(entry.text, "copied canary text");
    assert_eq!(entry.source.class, "kitty");
    assert_eq!(entry.source.workspace, "1");
}

#[test]
fn inject_without_payload_is_usage_error() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_capricorn-capture"))
        .args(["inject", "--class", "kitty", "--title", "x"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
