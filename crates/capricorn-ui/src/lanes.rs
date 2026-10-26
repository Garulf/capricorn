use capricorn_core::proto::{Kind, TimelineEntry, Ts};
use iced::mouse::{self, Cursor};
use iced::widget::canvas::{Action, Event, Frame, Geometry, Path, Program, Stroke, Text};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

use crate::format::{Clock, hour_label};
use crate::style;

pub const LANE_GAP_MS: Ts = 60_000;
pub const HEIGHT: f32 = 104.0;
const HOUR_MS: Ts = 3_600_000;
const MIN_SPAN_MS: Ts = 2 * HOUR_MS;

const LABEL_SIZE: f32 = 11.0;
const FOCUS_Y: f32 = 18.0;
const FOCUS_H: f32 = 22.0;
const CLIP_LABEL_Y: f32 = 48.0;
const CLIP_Y: f32 = 64.0;
const CLIP_H: f32 = 14.0;
const AXIS_Y: f32 = 86.0;

/// A stretch of time one app held focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub app: String,
    pub start: Ts,
    pub end: Ts,
}

/// Screen entries become coloured segments. A segment runs until the next capture unless
/// more than `LANE_GAP_MS` passed, which leaves a gap (idle, locked or paused).
pub fn lane_segments(entries: &[TimelineEntry]) -> Vec<Segment> {
    let mut screens: Vec<&TimelineEntry> =
        entries.iter().filter(|e| e.kind == Kind::Screen).collect();
    screens.sort_by_key(|e| e.ts);
    let mut segments: Vec<Segment> = Vec::new();
    for (index, entry) in screens.iter().enumerate() {
        let end = screens
            .get(index + 1)
            .map(|next| next.ts)
            .filter(|&next| next - entry.ts <= LANE_GAP_MS)
            .unwrap_or(entry.ts);
        match segments.last_mut() {
            Some(last) if last.app == entry.app && last.end == entry.ts => last.end = end,
            _ => segments.push(Segment {
                app: entry.app.clone(),
                start: entry.ts,
                end,
            }),
        }
    }
    segments
}

/// Whole hours around the day's activity, at least two hours wide, inside the day.
pub fn axis_range(entries: &[TimelineEntry], day: (Ts, Ts)) -> (Ts, Ts) {
    let (day_start, day_end) = day;
    let first = entries.iter().map(|e| e.ts).min();
    let last = entries.iter().map(|e| e.ts).max();
    let (Some(first), Some(last)) = (first, last) else {
        return (day_start + 8 * HOUR_MS, day_start + 18 * HOUR_MS);
    };
    let start = day_start + (first - day_start).div_euclid(HOUR_MS) * HOUR_MS;
    let end = day_start + (last - day_start + HOUR_MS - 1).div_euclid(HOUR_MS) * HOUR_MS;
    let end = end.max(start + MIN_SPAN_MS).min(day_end);
    let start = start.min(end - MIN_SPAN_MS).max(day_start);
    (start, end)
}

/// Read-only drawing of the focused-app and clipboard lanes with the time axis.
pub struct Lanes<'a, Message> {
    pub segments: &'a [Segment],
    pub entries: &'a [TimelineEntry],
    pub range: (Ts, Ts),
    pub cursor: Option<Ts>,
    pub matches: &'a [Ts],
    pub app_filter: Option<&'a str>,
    pub clock: &'a Clock,
    pub on_scrub: fn(Ts) -> Message,
    pub on_step: fn(i32) -> Message,
}

#[derive(Default)]
pub struct DragState {
    dragging: bool,
}

impl<Message> Lanes<'_, Message> {
    fn x_of(&self, ts: Ts, width: f32) -> f32 {
        let (start, end) = self.range;
        let span = (end - start).max(1) as f32;
        ((ts - start) as f32 / span * width).clamp(0.0, width)
    }

    fn ts_at(&self, x: f32, width: f32) -> Ts {
        let (start, end) = self.range;
        let fraction = (x / width.max(1.0)).clamp(0.0, 1.0);
        start + ((end - start) as f32 * fraction) as Ts
    }

    fn draw_label(frame: &mut Frame, content: &str, position: Point, colour: Color) {
        frame.fill_text(Text {
            content: content.to_string(),
            position,
            color: colour,
            size: LABEL_SIZE.into(),
            ..Text::default()
        });
    }

    fn draw_focus_lane(&self, frame: &mut Frame, width: f32) {
        Self::draw_label(
            frame,
            "FOCUSED APP",
            Point::new(0.0, 0.0),
            style::TEXT_FAINT,
        );
        let track = Path::rounded_rectangle(
            Point::new(0.0, FOCUS_Y),
            Size::new(width, FOCUS_H),
            4.0.into(),
        );
        frame.fill(&track, style::SURFACE_RAISED);
        for segment in self.segments {
            let x = self.x_of(segment.start, width);
            let w = (self.x_of(segment.end, width) - x).max(2.0);
            let dimmed = self.app_filter.is_some_and(|app| app != segment.app);
            let colour = style::app_colour(&segment.app);
            let colour = if dimmed {
                Color { a: 0.25, ..colour }
            } else {
                colour
            };
            frame.fill_rectangle(Point::new(x, FOCUS_Y), Size::new(w, FOCUS_H), colour);
        }
        for &ts in self.matches {
            let x = self.x_of(ts, width);
            frame.fill_rectangle(
                Point::new(x - 1.0, FOCUS_Y + FOCUS_H + 1.0),
                Size::new(3.0, 4.0),
                style::ACCENT,
            );
        }
    }

    fn draw_clipboard_lane(&self, frame: &mut Frame, width: f32) {
        Self::draw_label(
            frame,
            "CLIPBOARD",
            Point::new(0.0, CLIP_LABEL_Y),
            style::TEXT_FAINT,
        );
        let track = Path::rounded_rectangle(
            Point::new(0.0, CLIP_Y),
            Size::new(width, CLIP_H),
            4.0.into(),
        );
        frame.fill(&track, style::SURFACE_RAISED);
        for entry in self.entries.iter().filter(|e| e.kind == Kind::Clipboard) {
            let dot = Path::circle(
                Point::new(self.x_of(entry.ts, width), CLIP_Y + CLIP_H / 2.0),
                4.0,
            );
            frame.fill(&dot, style::CLIPBOARD_DOT);
        }
    }

    fn draw_axis(&self, frame: &mut Frame, width: f32) {
        let (start, end) = self.range;
        let hours = ((end - start) / HOUR_MS).max(1);
        let step = (hours as f32 / 6.0).ceil().max(1.0) as Ts * HOUR_MS;
        let mut tick = start;
        while tick <= end {
            let x = self.x_of(tick, width);
            let label = hour_label(tick, self.clock);
            let align = if tick == start {
                iced::alignment::Horizontal::Left
            } else if tick + step > end {
                iced::alignment::Horizontal::Right
            } else {
                iced::alignment::Horizontal::Center
            };
            frame.fill_text(Text {
                content: label,
                position: Point::new(x, AXIS_Y),
                color: style::TEXT_MUTED,
                size: LABEL_SIZE.into(),
                align_x: align.into(),
                ..Text::default()
            });
            tick += step;
        }
    }

    fn draw_cursor(&self, frame: &mut Frame, width: f32) {
        let Some(ts) = self.cursor else { return };
        let x = self.x_of(ts, width).clamp(1.0, width - 1.0);
        let line = Path::line(
            Point::new(x, FOCUS_Y - 3.0),
            Point::new(x, CLIP_Y + CLIP_H + 3.0),
        );
        frame.stroke(
            &line,
            Stroke::default().with_color(style::WARNING).with_width(2.0),
        );
    }
}

impl<Message> Program<Message> for Lanes<'_, Message> {
    type State = DragState;

    fn update(
        &self,
        state: &mut DragState,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<Action<Message>> {
        let scrub = |position: Point| {
            Action::publish((self.on_scrub)(self.ts_at(position.x, bounds.width))).and_capture()
        };
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let position = cursor.position_in(bounds)?;
                state.dragging = true;
                Some(scrub(position))
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                let position = cursor.position_from(bounds.position())?;
                Some(scrub(position))
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.dragging = false;
                None
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                cursor.position_in(bounds)?;
                let amount = match delta {
                    mouse::ScrollDelta::Lines { y, x } | mouse::ScrollDelta::Pixels { y, x } => {
                        if y.abs() >= x.abs() { *y } else { -*x }
                    }
                };
                let step = if amount > 0.0 { -1 } else { 1 };
                Some(Action::publish((self.on_step)(step)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &DragState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let width = bounds.width;
        self.draw_focus_lane(&mut frame, width);
        self.draw_clipboard_lane(&mut frame, width);
        self.draw_axis(&mut frame, width);
        self.draw_cursor(&mut frame, width);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &DragState,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_core::proto::RecordRef;

    fn entry(secs: i64, app: &str, kind: Kind) -> TimelineEntry {
        TimelineEntry {
            r: RecordRef {
                hour: 0,
                seq: secs as u32,
            },
            ts: secs * 1000,
            kind,
            app: app.into(),
        }
    }

    fn seg(app: &str, start: i64, end: i64) -> Segment {
        Segment {
            app: app.into(),
            start: start * 1000,
            end: end * 1000,
        }
    }

    #[test]
    fn timeline_lane_segments() {
        let entries = [
            entry(30, "firefox", Kind::Screen),
            entry(0, "kitty", Kind::Screen),
            entry(10, "kitty", Kind::Screen),
            entry(15, "firefox", Kind::Clipboard),
            entry(20, "firefox", Kind::Screen),
            entry(200, "firefox", Kind::Screen),
            entry(230, "firefox", Kind::Screen),
        ];
        assert_eq!(
            lane_segments(&entries),
            [
                seg("kitty", 0, 20),
                seg("firefox", 20, 30),
                seg("firefox", 200, 230),
            ]
        );
    }

    #[test]
    fn lone_capture_is_a_point_segment() {
        let entries = [entry(0, "kitty", Kind::Screen)];
        assert_eq!(lane_segments(&entries), [seg("kitty", 0, 0)]);
        assert!(lane_segments(&[]).is_empty());
    }

    #[test]
    fn axis_covers_activity_in_whole_hours() {
        let day = (0, 24 * HOUR_MS);
        let entries = [
            entry(9 * 3600 + 120, "kitty", Kind::Screen),
            entry(16 * 3600 + 5, "kitty", Kind::Screen),
        ];
        assert_eq!(axis_range(&entries, day), (9 * HOUR_MS, 17 * HOUR_MS));
        let late = [entry(23 * 3600 + 30, "kitty", Kind::Screen)];
        assert_eq!(axis_range(&late, day), (22 * HOUR_MS, 24 * HOUR_MS));
        assert_eq!(axis_range(&[], day), (8 * HOUR_MS, 18 * HOUR_MS));
    }
}
