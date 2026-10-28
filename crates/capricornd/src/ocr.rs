use std::path::Path;

use anyhow::{Context, Result};
use capricorn_core::proto::Rect;
use image::RgbImage;
use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
use rten::Model;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    pub rect: Rect,
}

pub trait Ocr: Send + Sync {
    /// Words in reading order, with axis-aligned boxes in image pixels.
    fn words(&self, img: &RgbImage) -> Result<Vec<Word>>;
}

/// Shareable between threads; `words` takes `&self`, so one engine serves all callers.
pub struct OcrsEngine {
    engine: OcrEngine,
}

impl OcrsEngine {
    pub fn load(models_dir: &Path) -> Result<Self> {
        let model = |name: &str| {
            let path = models_dir.join(name);
            Model::load_file(&path).with_context(|| format!("loading OCR model {}", path.display()))
        };
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(model("text-detection.onnx")?),
            recognition_model: Some(model("text-recognition.onnx")?),
            ..Default::default()
        })?;
        Ok(Self { engine })
    }
}

impl Ocr for OcrsEngine {
    fn words(&self, img: &RgbImage) -> Result<Vec<Word>> {
        let source = ImageSource::from_bytes(img.as_raw(), img.dimensions())?;
        let input = self.engine.prepare_input(source)?;
        let detected = self.engine.detect_words(&input)?;
        let lines = self.engine.find_text_lines(&input, &detected);
        let recognized = self.engine.recognize_text(&input, &lines)?;
        Ok(recognized
            .iter()
            .flatten()
            .flat_map(|line| line.words())
            .map(|word| {
                let bounds = word.bounding_rect();
                Word {
                    text: word.to_string(),
                    rect: Rect {
                        x: bounds.left(),
                        y: bounds.top(),
                        w: bounds.width().max(0) as u32,
                        h: bounds.height().max(0) as u32,
                    },
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs CAPRICORN_OCR_MODELS"]
    fn ocrs_reads_mockup() {
        let models = std::env::var("CAPRICORN_OCR_MODELS").expect("CAPRICORN_OCR_MODELS");
        let engine = OcrsEngine::load(Path::new(&models)).unwrap();
        let mockup = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mockup.png");
        let img = image::open(mockup).unwrap().to_rgb8();
        let started = std::time::Instant::now();
        let words = engine.words(&img).unwrap();
        eprintln!("ocrs: {} words in {:?}", words.len(), started.elapsed());
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        for expected in ["Quick", "search", "Timeline", "Clipboard"] {
            assert!(
                text.iter().any(|t| t.contains(expected)),
                "missing {expected} in {text:?}"
            );
        }
        let (w, h) = img.dimensions();
        assert!(
            words
                .iter()
                .all(|word| word.rect.x < w as i32 && word.rect.y < h as i32)
        );
    }
}
