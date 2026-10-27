use std::collections::HashMap;

use image::imageops::{self, FilterType};
use image::{GrayImage, RgbImage};

const GRID_COLUMNS: u32 = 160;
const GRID_ROWS: u32 = 90;
const CELL_CHANGE_THRESHOLD: u8 = 8;
/// A blinking cursor smears into a handful of cells; a new line of text touches dozens.
const MAX_CHANGED_CELLS: usize = 8;

/// Skips frames that differ from the last kept frame of the same output in at
/// most a handful of grid cells.
#[derive(Default)]
pub struct Dedupe {
    last_grid: HashMap<String, GrayImage>,
}

impl Dedupe {
    pub fn is_duplicate(&mut self, output: &str, img: &RgbImage) -> bool {
        let grid = grid(img);
        let duplicate = self
            .last_grid
            .get(output)
            .is_some_and(|last| changed_cells(last, &grid) <= MAX_CHANGED_CELLS);
        if !duplicate {
            self.last_grid.insert(output.to_owned(), grid);
        }
        duplicate
    }
}

fn grid(img: &RgbImage) -> GrayImage {
    let gray = imageops::grayscale(img);
    imageops::resize(&gray, GRID_COLUMNS, GRID_ROWS, FilterType::Triangle)
}

fn changed_cells(a: &GrayImage, b: &GrayImage) -> usize {
    a.as_raw()
        .iter()
        .zip(b.as_raw())
        .filter(|(x, y)| x.abs_diff(**y) > CELL_CHANGE_THRESHOLD)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;

    fn blank_terminal() -> RgbImage {
        RgbImage::from_pixel(WIDTH, HEIGHT, Rgb([20, 20, 20]))
    }

    fn fill(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, color: Rgb<u8>) {
        for py in y..y + h {
            for px in x..x + w {
                img.put_pixel(px, py, color);
            }
        }
    }

    fn type_line(img: &mut RgbImage, row: u32, chars: u32) {
        let glyph = Rgb([220, 220, 220]);
        for i in 0..chars {
            fill(img, 10 + i * 10, row * 20 + 4, 6, 12, glyph);
        }
    }

    #[test]
    fn first_frame_is_not_duplicate() {
        let mut dedupe = Dedupe::default();
        assert!(!dedupe.is_duplicate("DP-1", &blank_terminal()));
    }

    #[test]
    fn identical_frame_is_duplicate() {
        let mut dedupe = Dedupe::default();
        dedupe.is_duplicate("DP-1", &blank_terminal());
        assert!(dedupe.is_duplicate("DP-1", &blank_terminal()));
    }

    #[test]
    fn blinking_cursor_is_duplicate() {
        let mut dedupe = Dedupe::default();
        let mut frame = blank_terminal();
        dedupe.is_duplicate("DP-1", &frame);
        fill(&mut frame, 10, 4, 8, 16, Rgb([220, 220, 220]));
        assert!(dedupe.is_duplicate("DP-1", &frame));
    }

    #[test]
    fn new_terminal_line_is_kept() {
        let mut dedupe = Dedupe::default();
        let mut frame = blank_terminal();
        type_line(&mut frame, 0, 40);
        dedupe.is_duplicate("DP-1", &frame);
        type_line(&mut frame, 1, 30);
        assert!(!dedupe.is_duplicate("DP-1", &frame));
    }

    #[test]
    fn outputs_are_tracked_separately() {
        let mut dedupe = Dedupe::default();
        dedupe.is_duplicate("DP-1", &blank_terminal());
        assert!(!dedupe.is_duplicate("HDMI-A-1", &blank_terminal()));
    }
}
