use std::sync::{Arc, LazyLock};

use anyhow::{Result, bail};
use capricorn_core::Record;
use capricorn_core::proto::{ClipboardEntry, Frame, Kind, WindowContext};
use capricorn_core::secrets::{find_secrets, is_secret};
use image::RgbImage;
use image::imageops::{self, FilterType};
use regex::Regex;

use crate::dedupe::Dedupe;
use crate::ocr::{Ocr, Word};
use crate::redact::{redact, redact_text};

const IMAGE_QUALITY: f32 = 75.0;
const THUMB_QUALITY: f32 = 60.0;
const THUMB_WIDTH: u32 = 320;
const WEBP_MAX_DIMENSION: u32 = 16383;
const ADDRESS_BAR_BAND: f64 = 0.15;
const BROWSER_CLASSES: &[&str] = &[
    "firefox",
    "chromium",
    "google-chrome",
    "brave",
    "librewolf",
    "zen",
];

static URL_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(https?://)?[\w.-]+\.[a-z]{2,}(/\S*)?$").expect("url pattern compiles")
});

/// First URL-looking word in the top band of a browser window, as shown in its address bar.
pub fn guess_url(class: &str, words: &[Word], image_height: u32) -> Option<String> {
    if !BROWSER_CLASSES
        .iter()
        .any(|browser| class.eq_ignore_ascii_case(browser))
    {
        return None;
    }
    let band = f64::from(image_height) * ADDRESS_BAR_BAND;
    words
        .iter()
        .filter(|word| f64::from(word.rect.y) + f64::from(word.rect.h) / 2.0 < band)
        .find(|word| URL_TOKEN.is_match(&word.text))
        .map(|word| word.text.clone())
}

pub struct Pipeline {
    ocr: Arc<dyn Ocr>,
    dedupe: Dedupe,
    extra: Vec<Regex>,
}

impl Pipeline {
    pub fn new(ocr: Arc<dyn Ocr>, extra: Vec<Regex>) -> Self {
        Self {
            ocr,
            dedupe: Dedupe::default(),
            extra,
        }
    }

    /// `None` when the frame is a near duplicate of the previous one on its output.
    pub fn frame(&mut self, frame: Frame) -> Result<Option<Record>> {
        let mut img = rgb_image(&frame)?;
        if self.dedupe.is_duplicate(&frame.output, &img) {
            return Ok(None);
        }
        let words = self.ocr.words(&img)?;
        let url = guess_url(&frame.window.class, &words, img.height())
            .filter(|url| find_secrets(url, &self.extra).is_empty());
        let text = redact(&mut img, &words, &self.extra);
        Ok(Some(Record {
            ts: frame.ts,
            kind: Kind::Screen,
            window: self.scrubbed(frame.window),
            text,
            url,
            image: Some(encode_webp(&img, IMAGE_QUALITY)?),
            thumb: Some(encode_webp(&thumbnail(&img), THUMB_QUALITY)?),
        }))
    }

    /// `None` when the text is empty or contains a secret.
    pub fn clipboard(&self, entry: ClipboardEntry) -> Option<Record> {
        if entry.text.trim().is_empty() || is_secret(&entry.text, &self.extra) {
            return None;
        }
        Some(Record {
            ts: entry.ts,
            kind: Kind::Clipboard,
            window: self.scrubbed(entry.source),
            text: entry.text,
            ..Record::default()
        })
    }
}

impl Pipeline {
    /// Window titles and working directories can show tokens too (a terminal running
    /// `curl -H "Authorization: ..."`), so they get the same replacement as OCR text.
    fn scrubbed(&self, mut window: WindowContext) -> WindowContext {
        window.title = redact_text(&window.title, &self.extra);
        window.cwd = window.cwd.map(|cwd| redact_text(&cwd, &self.extra));
        window
    }
}

fn rgb_image(frame: &Frame) -> Result<RgbImage> {
    let (width, height) = (frame.width, frame.height);
    if width == 0 || height == 0 || width > WEBP_MAX_DIMENSION || height > WEBP_MAX_DIMENSION {
        bail!("unsupported frame size {width}x{height}");
    }
    let expected = width as usize * height as usize * 4;
    if frame.rgba.len() != expected {
        bail!(
            "frame buffer is {} bytes, expected {expected}",
            frame.rgba.len()
        );
    }
    let rgb = frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|[r, g, b, _alpha]| [*r, *g, *b])
        .collect();
    Ok(RgbImage::from_raw(width, height, rgb).expect("buffer length checked"))
}

fn thumbnail(img: &RgbImage) -> RgbImage {
    if img.width() <= THUMB_WIDTH {
        return img.clone();
    }
    let height = (u64::from(img.height()) * u64::from(THUMB_WIDTH) / u64::from(img.width())).max(1);
    imageops::resize(img, THUMB_WIDTH, height as u32, FilterType::Triangle)
}

fn encode_webp(img: &RgbImage, quality: f32) -> Result<Vec<u8>> {
    let encoded = webp::Encoder::from_rgb(img.as_raw(), img.width(), img.height())
        .encode_simple(false, quality)
        .map_err(|e| anyhow::anyhow!("webp encoding failed: {e:?}"))?;
    Ok(encoded.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_core::proto::{Kind, Rect, WindowContext};
    use image::{ImageFormat, Rgb, RgbImage};

    struct FakeOcr(Vec<Word>);

    impl Ocr for FakeOcr {
        fn words(&self, _img: &RgbImage) -> Result<Vec<Word>> {
            Ok(self.0.clone())
        }
    }

    fn word(text: &str, x: i32, y: i32, w: u32) -> Word {
        Word {
            text: text.into(),
            rect: Rect { x, y, w, h: 16 },
        }
    }

    fn github_token() -> String {
        format!("{}{}", "ghp_", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6")
    }

    fn pipeline(words: Vec<Word>) -> Pipeline {
        Pipeline::new(Arc::new(FakeOcr(words)), Vec::new())
    }

    fn frame(width: u32, height: u32, reversed: bool) -> Frame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _y in 0..height {
            for x in 0..width {
                let ramp = (x * 255 / (width - 1)) as u8;
                let v = if reversed { 255 - ramp } else { ramp };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        Frame {
            ts: 1_700_000_000_000,
            output: "DP-1".into(),
            width,
            height,
            rgba,
            window: WindowContext {
                class: "kitty".into(),
                ..WindowContext::default()
            },
        }
    }

    fn decode_webp(bytes: &[u8]) -> RgbImage {
        image::load_from_memory_with_format(bytes, ImageFormat::WebP)
            .unwrap()
            .to_rgb8()
    }

    fn clipboard(text: &str) -> ClipboardEntry {
        ClipboardEntry {
            ts: 5,
            text: text.into(),
            source: WindowContext::default(),
        }
    }

    #[test]
    fn duplicate_frame_skipped() {
        let mut p = pipeline(vec![word("hello", 5, 5, 40)]);
        assert!(p.frame(frame(640, 400, false)).unwrap().is_some());
        assert!(p.frame(frame(640, 400, false)).unwrap().is_none());
    }

    #[test]
    fn different_frame_kept() {
        let mut p = pipeline(vec![word("hello", 5, 5, 40)]);
        assert!(p.frame(frame(640, 400, false)).unwrap().is_some());
        assert!(p.frame(frame(640, 400, true)).unwrap().is_some());
    }

    #[test]
    fn frame_record_carries_metadata_and_text() {
        let mut p = pipeline(vec![word("hello", 5, 5, 40), word("world", 60, 5, 40)]);
        let record = p.frame(frame(640, 400, false)).unwrap().unwrap();
        assert_eq!(record.kind, Kind::Screen);
        assert_eq!(record.ts, 1_700_000_000_000);
        assert_eq!(record.window.class, "kitty");
        assert_eq!(record.text, "hello world");
        assert_eq!(record.url, None);
    }

    #[test]
    fn secret_on_screen_blurred_and_redacted() {
        let token = github_token();
        let words = vec![word("export", 5, 100, 60), word(&token, 80, 100, 300)];
        let mut p = pipeline(words);
        let original = frame(640, 400, false);
        let record = p.frame(original.clone()).unwrap().unwrap();
        assert!(record.text.contains("[redacted]"));
        assert!(!record.text.contains(&token));
        let stored = decode_webp(record.image.as_ref().unwrap());
        let before = RgbImage::from_fn(640, 400, |x, _| {
            let i = ((108 * 640 + x) * 4) as usize;
            Rgb([original.rgba[i], original.rgba[i + 1], original.rgba[i + 2]])
        });
        let inside = stored.get_pixel(300, 108);
        assert!(
            inside.0.iter().all(|c| *c < 80),
            "redacted box is dark, got {inside:?}"
        );
        assert!(
            before.get_pixel(300, 108).0[0] > 80,
            "fixture is bright there"
        );
    }

    #[test]
    fn webp_and_thumb_decode() {
        let mut p = pipeline(Vec::new());
        let record = p.frame(frame(640, 400, false)).unwrap().unwrap();
        let image = decode_webp(record.image.as_ref().unwrap());
        assert_eq!(image.dimensions(), (640, 400));
        let thumb = decode_webp(record.thumb.as_ref().unwrap());
        assert_eq!(thumb.dimensions(), (320, 200));
    }

    #[test]
    fn mismatched_buffer_is_an_error() {
        let mut p = pipeline(Vec::new());
        let mut bad = frame(64, 64, false);
        bad.rgba.truncate(100);
        assert!(p.frame(bad).is_err());
    }

    #[test]
    fn clipboard_secret_dropped() {
        let p = pipeline(Vec::new());
        assert!(
            p.clipboard(clipboard(&format!("export T={}", github_token())))
                .is_none()
        );
    }

    #[test]
    fn secrets_in_title_and_cwd_are_redacted() {
        let token = github_token();
        let mut p = pipeline(Vec::new());
        let mut f = frame(64, 32, false);
        f.window.title = format!("curl -H {token}");
        f.window.cwd = Some(format!("/tmp/{token}"));
        let record = p.frame(f).unwrap().unwrap();
        assert_eq!(record.window.title, "curl -H [redacted]");
        assert_eq!(record.window.cwd.as_deref(), Some("/tmp/[redacted]"));

        let mut entry = clipboard("plain text");
        entry.source.title = format!("token {token}");
        let clip = p.clipboard(entry).unwrap();
        assert_eq!(clip.window.title, "token [redacted]");
    }

    #[test]
    fn clipboard_normal_kept() {
        let p = pipeline(Vec::new());
        let record = p.clipboard(clipboard("nix flake update")).unwrap();
        assert_eq!(record.kind, Kind::Clipboard);
        assert_eq!(record.text, "nix flake update");
        assert_eq!(record.ts, 5);
        assert!(record.image.is_none());
    }

    #[test]
    fn url_guess_firefox() {
        let words = vec![
            word("Mozilla", 10, 8, 60),
            word("github.com/Garulf/capricorn", 100, 40, 300),
            word("lower.example.org", 100, 800, 200),
        ];
        assert_eq!(
            guess_url("firefox", &words, 1000).as_deref(),
            Some("github.com/Garulf/capricorn")
        );
    }

    #[test]
    fn url_guess_ignores_below_the_top_band() {
        let words = vec![word("lower.example.org", 100, 800, 200)];
        assert_eq!(guess_url("firefox", &words, 1000), None);
    }

    #[test]
    fn url_guess_ignores_non_browser() {
        let words = vec![word("github.com/Garulf/capricorn", 100, 40, 300)];
        assert_eq!(guess_url("kitty", &words, 1000), None);
    }

    #[test]
    fn url_in_frame_is_stored_for_browsers() {
        let mut p = pipeline(vec![word("https://nixos.org/manual", 10, 10, 200)]);
        let mut f = frame(640, 400, false);
        f.window.class = "firefox".into();
        let record = p.frame(f).unwrap().unwrap();
        assert_eq!(record.url.as_deref(), Some("https://nixos.org/manual"));
    }

    #[test]
    fn url_carrying_a_secret_is_not_stored() {
        let url = format!("example.com/?t={}", github_token());
        let mut p = pipeline(vec![word(&url, 10, 10, 400)]);
        let mut f = frame(640, 400, false);
        f.window.class = "firefox".into();
        assert_eq!(p.frame(f).unwrap().unwrap().url, None);
    }
}
