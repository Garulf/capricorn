use std::collections::HashMap;

use image::RgbImage;
use image::imageops::{self, FilterType};

const MAX_HAMMING_DISTANCE: u32 = 3;
const HASH_COLUMNS: u32 = 9;
const HASH_ROWS: u32 = 8;

/// Skips frames whose 64-bit dHash is within a few bits of the last kept frame of the same output.
#[derive(Default)]
pub struct Dedupe {
    last_hash: HashMap<String, u64>,
}

impl Dedupe {
    pub fn is_duplicate(&mut self, output: &str, img: &RgbImage) -> bool {
        let hash = dhash(img);
        let duplicate = self
            .last_hash
            .get(output)
            .is_some_and(|last| (last ^ hash).count_ones() <= MAX_HAMMING_DISTANCE);
        if !duplicate {
            self.last_hash.insert(output.to_owned(), hash);
        }
        duplicate
    }
}

fn dhash(img: &RgbImage) -> u64 {
    let gray = imageops::grayscale(img);
    let small = imageops::resize(&gray, HASH_COLUMNS, HASH_ROWS, FilterType::Triangle);
    let mut hash = 0u64;
    for y in 0..HASH_ROWS {
        for x in 0..HASH_COLUMNS - 1 {
            hash <<= 1;
            hash |= u64::from(small.get_pixel(x, y)[0] < small.get_pixel(x + 1, y)[0]);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn horizontal_gradient(reversed: bool) -> RgbImage {
        RgbImage::from_fn(64, 48, |x, _| {
            let v = if reversed { 255 - x * 4 } else { x * 4 } as u8;
            Rgb([v, v, v])
        })
    }

    #[test]
    fn first_frame_is_not_duplicate() {
        let mut dedupe = Dedupe::default();
        assert!(!dedupe.is_duplicate("DP-1", &horizontal_gradient(false)));
    }

    #[test]
    fn identical_frame_is_duplicate() {
        let mut dedupe = Dedupe::default();
        dedupe.is_duplicate("DP-1", &horizontal_gradient(false));
        assert!(dedupe.is_duplicate("DP-1", &horizontal_gradient(false)));
    }

    #[test]
    fn small_noise_is_duplicate() {
        let mut dedupe = Dedupe::default();
        let mut noisy = horizontal_gradient(false);
        dedupe.is_duplicate("DP-1", &noisy);
        noisy.put_pixel(3, 3, Rgb([255, 0, 0]));
        assert!(dedupe.is_duplicate("DP-1", &noisy));
    }

    #[test]
    fn different_frame_is_kept() {
        let mut dedupe = Dedupe::default();
        dedupe.is_duplicate("DP-1", &horizontal_gradient(false));
        assert!(!dedupe.is_duplicate("DP-1", &horizontal_gradient(true)));
    }

    #[test]
    fn outputs_are_tracked_separately() {
        let mut dedupe = Dedupe::default();
        dedupe.is_duplicate("DP-1", &horizontal_gradient(false));
        assert!(!dedupe.is_duplicate("HDMI-A-1", &horizontal_gradient(false)));
    }
}
