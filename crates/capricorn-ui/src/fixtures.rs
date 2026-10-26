//! Fake daemon data shaped like the agreed mockup.

use capricorn_core::proto::{Hit, Kind, RecordDetail, RecordRef, TimelineEntry, Ts};
use jiff::civil::date;
use jiff::tz::TimeZone;

use crate::format::Clock;

pub fn clock() -> Clock {
    let tz = TimeZone::fixed(jiff::tz::offset(1));
    let now = local(&tz, 27, 18, 0, 0);
    Clock { now, tz }
}

fn local(tz: &TimeZone, day: i8, hour: i8, minute: i8, second: i8) -> Ts {
    date(2026, 10, day)
        .at(hour, minute, second, 0)
        .to_zoned(tz.clone())
        .unwrap()
        .timestamp()
        .as_millisecond()
}

pub fn at(day: i8, hour: i8, minute: i8, second: i8) -> Ts {
    local(&clock().tz, day, hour, minute, second)
}

fn record_ref(ts: Ts) -> RecordRef {
    let secs = ts / 1000;
    RecordRef {
        hour: (secs / 3600) as u32,
        seq: (secs % 3600) as u32,
    }
}

/// Byte ranges of every occurrence of each term.
fn highlights(snippet: &str, terms: &[&str]) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = terms
        .iter()
        .flat_map(|term| {
            snippet
                .match_indices(term)
                .map(|(start, m)| (start as u32, (start + m.len()) as u32))
        })
        .collect();
    ranges.sort_unstable();
    ranges
}

fn hit(
    ts: Ts,
    kind: Kind,
    app: &str,
    workspace: &str,
    context: Option<&str>,
    snippet: &str,
) -> Hit {
    Hit {
        r: record_ref(ts),
        ts,
        kind,
        app: app.into(),
        title: String::new(),
        workspace: workspace.into(),
        context: context.map(str::to_string),
        snippet: snippet.into(),
        highlights: highlights(snippet, &["nix flake", "flake lock", "lock", "update"]),
    }
}

pub fn hits() -> Vec<Hit> {
    vec![
        hit(
            at(27, 14, 32, 8),
            Kind::Screen,
            "kitty",
            "2",
            Some("~/projects/dotfiles"),
            "$ nix flake lock --update-input nixpkgs",
        ),
        hit(
            at(26, 9, 10, 0),
            Kind::Clipboard,
            "firefox",
            "3",
            None,
            "nix flake update --commit-lock-file",
        ),
        hit(
            at(26, 9, 8, 0),
            Kind::Screen,
            "firefox",
            "3",
            Some("wiki.nixos.org"),
            "NixOS Wiki: Flakes. \"...run nix flake update to refresh the lock...\"",
        ),
        hit(
            at(25, 21, 47, 0),
            Kind::Screen,
            "vesktop",
            "4",
            Some("#nixos"),
            "\"just update the flake lock and rebuild\"",
        ),
    ]
}

const DAY_PLAN: [(i8, i8, &str); 7] = [
    (9, 0, "kitty"),
    (10, 0, "firefox"),
    (11, 40, "neovim"),
    (12, 50, "kitty"),
    (14, 40, "vesktop"),
    (15, 50, "firefox"),
    (17, 0, ""),
];
const LUNCH: (Ts, Ts) = (12 * 60 + 20, 12 * 60 + 50);
const CLIPBOARD_TIMES: [(i8, i8); 5] = [(9, 25), (11, 10), (11, 20), (14, 50), (16, 20)];

/// A working day: one screen capture every 30 s, a lunch gap and a few copies.
pub fn day_entries() -> Vec<TimelineEntry> {
    let mut entries = Vec::new();
    for pair in DAY_PLAN.windows(2) {
        let (start_h, start_m, app) = pair[0];
        let (end_h, end_m, _) = pair[1];
        let mut minute_ms = at(27, start_h, start_m, 0);
        let end = at(27, end_h, end_m, 0);
        while minute_ms < end {
            let minute_of_day = (minute_ms - at(27, 0, 0, 0)) / 60_000;
            if !(LUNCH.0..LUNCH.1).contains(&minute_of_day) {
                entries.push(entry(minute_ms, Kind::Screen, app));
            }
            minute_ms += 30_000;
        }
    }
    entries.push(entry(at(27, 14, 32, 8), Kind::Screen, "kitty"));
    for (hour, minute) in CLIPBOARD_TIMES {
        entries.push(entry(at(27, hour, minute, 5), Kind::Clipboard, "firefox"));
    }
    entries
}

fn entry(ts: Ts, kind: Kind, app: &str) -> TimelineEntry {
    TimelineEntry {
        r: record_ref(ts),
        ts,
        kind,
        app: app.into(),
    }
}

pub fn detail(entry: &TimelineEntry, workspace: &str) -> RecordDetail {
    let context = (entry.app == "kitty").then(|| "~/projects/dotfiles".to_string());
    RecordDetail {
        hit: Hit {
            r: entry.r,
            ts: entry.ts,
            kind: entry.kind,
            app: entry.app.clone(),
            title: String::new(),
            workspace: workspace.into(),
            context: context.clone(),
            snippet: String::new(),
            highlights: Vec::new(),
        },
        text: "$ nix flake lock --update-input nixpkgs".into(),
        cwd: context,
        url: None,
    }
}

/// A terminal-like screenshot drawn in code, RGBA.
pub fn fake_screenshot(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&screenshot_pixel(x, y, width, height));
        }
    }
    pixels
}

fn screenshot_pixel(x: u32, y: u32, width: u32, height: u32) -> [u8; 4] {
    let title_bar = height / 24;
    if y < title_bar {
        return [0x24, 0x28, 0x33, 0xff];
    }
    let pitch = (height / 36).max(3);
    let line = (y - title_bar) / pitch;
    let indent = width / 32;
    let length = width / 5 + (line * 97 % 11) * width / 22;
    let glyph_column = (x / (pitch / 2).max(1)) % 7 != 6;
    let in_text = (y - title_bar) % pitch < pitch / 3 && x > indent && x < indent + length;
    match (in_text && glyph_column, line % 5) {
        (true, 0) => [0x6a, 0xa8, 0x74, 0xff],
        (true, 3) => [0x5b, 0x8d, 0xef, 0xff],
        (true, _) => [0xa8, 0xb0, 0xbd, 0xff],
        (false, _) => [0x14, 0x17, 0x1e, 0xff],
    }
}
