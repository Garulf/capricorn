use std::ops::Range;

use capricorn_core::proto::Rect;
use capricorn_core::secrets::find_secrets;
use image::{Rgb, RgbImage};
use regex::Regex;

use crate::ocr::Word;

const BLUR_RADIUS: u32 = 12;
const BLUR_MARGIN: u32 = 4;
const FILL: Rgb<u8> = Rgb([24, 24, 24]);
const REDACTED: &str = "[redacted]";

/// Blanks every word that overlaps a secret and returns the page text with secrets replaced.
pub fn redact(img: &mut RgbImage, words: &[Word], extra: &[Regex]) -> String {
    let (text, spans) = join_words(words);
    let secrets = find_secrets(&text, extra);
    for (word, span) in words.iter().zip(&spans) {
        if secrets.iter().any(|secret| overlaps(secret, span)) {
            obscure(img, word.rect);
        }
    }
    replace_ranges(&text, &secrets)
}

/// Words joined by a space, or a newline when the next word starts a new visual line.
/// Returns the text and each word's byte span in it.
/// `text` with every secret span replaced by `[redacted]`.
pub fn redact_text(text: &str, extra: &[Regex]) -> String {
    replace_ranges(text, &find_secrets(text, extra))
}

fn join_words(words: &[Word]) -> (String, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut spans = Vec::with_capacity(words.len());
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            text.push(if starts_new_line(&words[i - 1].rect, &word.rect) {
                '\n'
            } else {
                ' '
            });
        }
        let start = text.len();
        text.push_str(&word.text);
        spans.push(start..text.len());
    }
    (text, spans)
}

fn starts_new_line(prev: &Rect, next: &Rect) -> bool {
    let top = prev.y.max(next.y);
    let bottom = (prev.y + prev.h as i32).min(next.y + next.h as i32);
    let overlap = (bottom - top).max(0) as u32;
    next.x < prev.x || overlap * 2 < prev.h.min(next.h)
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn replace_ranges(text: &str, ranges: &[Range<usize>]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in ranges {
        out.push_str(&text[cursor..range.start]);
        out.push_str(REDACTED);
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn obscure(img: &mut RgbImage, rect: Rect) {
    let Some(area) = clamp(img, rect, 0) else {
        return;
    };
    if let Some(soft) = clamp(img, rect, BLUR_MARGIN) {
        box_blur(img, soft, BLUR_RADIUS);
    }
    for y in area.1..area.3 {
        for x in area.0..area.2 {
            img.put_pixel(x, y, FILL);
        }
    }
}

/// `(x0, y0, x1, y1)` of `rect` grown by `margin` and clipped to the image, or `None` when empty.
fn clamp(img: &RgbImage, rect: Rect, margin: u32) -> Option<(u32, u32, u32, u32)> {
    let margin = margin as i64;
    let x0 = (rect.x as i64 - margin).clamp(0, img.width() as i64);
    let y0 = (rect.y as i64 - margin).clamp(0, img.height() as i64);
    let x1 = (rect.x as i64 + rect.w as i64 + margin).clamp(0, img.width() as i64);
    let y1 = (rect.y as i64 + rect.h as i64 + margin).clamp(0, img.height() as i64);
    (x0 < x1 && y0 < y1).then_some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

fn box_blur(img: &mut RgbImage, area: (u32, u32, u32, u32), radius: u32) {
    let (x0, y0, x1, y1) = area;
    let (width, height) = (x1 - x0, y1 - y0);
    let source = image::imageops::crop_imm(img, x0, y0, width, height).to_image();
    for y in 0..height {
        for x in 0..width {
            let mut sum = [0u32; 3];
            let mut count = 0;
            for sy in y.saturating_sub(radius)..(y + radius + 1).min(height) {
                for sx in x.saturating_sub(radius)..(x + radius + 1).min(width) {
                    let px = source.get_pixel(sx, sy);
                    for c in 0..3 {
                        sum[c] += u32::from(px[c]);
                    }
                    count += 1;
                }
            }
            img.put_pixel(x0 + x, y0 + y, Rgb(sum.map(|s| (s / count) as u8)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_core::proto::Rect;
    use image::Rgb;

    fn word(text: &str, x: i32, y: i32, w: u32) -> Word {
        Word {
            text: text.into(),
            rect: Rect { x, y, w, h: 16 },
        }
    }

    fn github_token() -> String {
        format!(
            "{}{}",
            "ghp_",
            "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6"
                .chars()
                .take(36)
                .collect::<String>()
        )
    }

    fn bright_image() -> RgbImage {
        RgbImage::from_pixel(400, 100, Rgb([230, 230, 230]))
    }

    #[test]
    fn secret_word_is_filled_and_text_redacted() {
        let token = github_token();
        let words = vec![word("token", 10, 20, 50), word(&token, 70, 20, 200)];
        let mut img = bright_image();
        let text = redact(&mut img, &words, &[]);
        assert_eq!(text, "token [redacted]");
        assert_ne!(*img.get_pixel(150, 28), Rgb([230, 230, 230]));
        assert_eq!(*img.get_pixel(30, 28), Rgb([230, 230, 230]));
    }

    #[test]
    fn secret_inside_longer_word_redacts_the_whole_word_box() {
        let token = github_token();
        let words = vec![word(&format!("TOKEN={token}"), 10, 20, 300)];
        let mut img = bright_image();
        let text = redact(&mut img, &words, &[]);
        assert_eq!(text, "TOKEN=[redacted]");
        assert_ne!(*img.get_pixel(20, 28), Rgb([230, 230, 230]));
    }

    #[test]
    fn secret_spanning_words_covers_every_word() {
        let begin = format!("-----BEGIN {} KEY-----", "PRIVATE");
        let end = format!("-----END {} KEY-----", "PRIVATE");
        let words = vec![
            word(&begin, 10, 10, 150),
            word("MIIEvQIBADANBgkq", 10, 30, 150),
            word(&end, 10, 50, 150),
            word("after", 10, 70, 50),
        ];
        let mut img = bright_image();
        let text = redact(&mut img, &words, &[]);
        assert_eq!(text, "[redacted]\nafter");
        assert_ne!(*img.get_pixel(20, 38), Rgb([230, 230, 230]));
        assert_eq!(*img.get_pixel(20, 78), Rgb([230, 230, 230]));
    }

    #[test]
    fn extra_patterns_apply() {
        let words = vec![word("project-falcon", 10, 20, 100)];
        let mut img = bright_image();
        let extra = [Regex::new("falcon").unwrap()];
        assert_eq!(redact(&mut img, &words, &extra), "project-[redacted]");
    }

    #[test]
    fn plain_text_is_untouched() {
        let words = vec![
            word("hello", 10, 20, 50),
            word("world", 70, 20, 50),
            word("next", 10, 50, 50),
        ];
        let mut img = bright_image();
        let before = img.clone();
        assert_eq!(redact(&mut img, &words, &[]), "hello world\nnext");
        assert_eq!(img, before);
    }

    #[test]
    fn rect_partly_outside_image_does_not_panic() {
        let token = github_token();
        let words = vec![word(&token, -20, 90, 600)];
        let mut img = bright_image();
        redact(&mut img, &words, &[]);
    }
}
