use capricorn_core::proto::{Hit, Kind, RecorderState, Status, VaultState};
use serde_json::{Value, json};

use crate::waybar::format_duration;

pub fn hit_json(hit: &Hit) -> Value {
    json!({
        "ref": hit.r.to_string(),
        "ts": hit.ts,
        "kind": kind_name(hit.kind),
        "app": hit.app,
        "title": hit.title,
        "workspace": hit.workspace,
        "context": hit.context,
        "snippet": hit.snippet,
        "highlights": hit.highlights,
    })
}

pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Screen => "screen",
        Kind::Clipboard => "clipboard",
    }
}

/// Two lines: reference, time, app and title, then the snippet.
pub fn hit_text(hit: &Hit, color: bool) -> String {
    format!(
        "{}  {}  {}  {}\n    {}",
        hit.r,
        format_ts(hit.ts),
        printable(&hit.app),
        printable(&hit.title),
        snippet(hit, color),
    )
}

fn snippet(hit: &Hit, color: bool) -> String {
    let text = printable(&hit.snippet);
    if color {
        highlight(&text, &hit.highlights)
    } else {
        text
    }
}

/// Captured text may contain terminal escapes. Controls become spaces of the same byte
/// length so highlight offsets stay valid.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                " ".repeat(c.len_utf8())
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn highlight(text: &str, ranges: &[(u32, u32)]) -> String {
    const START: &str = "\u{1b}[1m";
    const END: &str = "\u{1b}[0m";
    let mut ranges = ranges.to_vec();
    ranges.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in ranges {
        let (start, end) = (start as usize, end as usize);
        let Some(marked) = text
            .get(start..end)
            .filter(|_| start >= cursor && start < end)
        else {
            continue;
        };
        out.push_str(&text[cursor..start]);
        out.push_str(START);
        out.push_str(marked);
        out.push_str(END);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// `YYYY-MM-DD HH:MM UTC` from unix milliseconds.
pub fn format_ts(ts_ms: i64) -> String {
    let secs = ts_ms.div_euclid(1000);
    let (days, secs_of_day) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        secs_of_day / 3600,
        secs_of_day % 3600 / 60
    )
}

/// Days since 1970-01-01 to proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn status_text(status: &Status) -> String {
    let vault = match status.vault {
        VaultState::Uninitialized => "not set up".to_owned(),
        VaultState::Locked => "locked".to_owned(),
        VaultState::Unlocked { locks_in_secs } => {
            format!("unlocked (locks in {})", format_duration(locks_in_secs))
        }
    };
    let recorder = match status.recorder {
        Some(RecorderState::Recording) => "recording",
        Some(RecorderState::Paused) => "paused",
        Some(RecorderState::Idle) => "idle",
        Some(RecorderState::Locked) => "locked",
        None => "not connected",
    };
    let mut text = format!("vault: {vault}\nrecorder: {recorder}");
    if status.pending_index > 0 {
        text.push_str(&format!("\nindexing: {} pending", status.pending_index));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_core::proto::RecordRef;

    fn sample_hit() -> Hit {
        Hit {
            r: RecordRef {
                hour: 491_000,
                seq: 7,
            },
            ts: 1_700_000_000_000,
            kind: Kind::Screen,
            app: "firefox".into(),
            title: "notes".into(),
            workspace: "3".into(),
            context: None,
            snippet: "the quarterly report".into(),
            highlights: vec![(4, 13)],
        }
    }

    #[test]
    fn timestamps_format_as_utc() {
        assert_eq!(format_ts(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_ts(1_700_000_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_ts(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(format_ts(-1000), "1969-12-31 23:59 UTC");
    }

    #[test]
    fn highlights_only_when_colour_is_on() {
        let hit = sample_hit();
        assert!(hit_text(&hit, false).ends_with("the quarterly report"));
        assert!(hit_text(&hit, true).contains("the \u{1b}[1mquarterly\u{1b}[0m report"));
    }

    #[test]
    fn bad_highlight_ranges_are_ignored() {
        let mut hit = sample_hit();
        hit.snippet = "héllo".into();
        hit.highlights = vec![(2, 3), (40, 50), (3, 1)];
        assert_eq!(snippet(&hit, true), "héllo");
    }

    #[test]
    fn terminal_escapes_in_captured_text_are_neutralised() {
        let mut hit = sample_hit();
        hit.snippet = "a\u{1b}[31mb\u{85}c".into();
        hit.highlights = vec![];
        let shown = snippet(&hit, false);
        assert!(!shown.contains('\u{1b}') && !shown.contains('\u{85}'));
        assert_eq!(shown.len(), hit.snippet.len());
    }

    #[test]
    fn json_uses_string_ref_and_lowercase_kind() {
        let json = hit_json(&sample_hit());
        assert_eq!(json["ref"], "491000:7");
        assert_eq!(json["kind"], "screen");
        assert_eq!(json["highlights"][0][1], 13);
    }

    #[test]
    fn status_text_lists_each_part() {
        let status = Status {
            vault: VaultState::Unlocked { locks_in_secs: 90 },
            recorder: Some(RecorderState::Paused),
            pending_index: 3,
        };
        assert_eq!(
            status_text(&status),
            "vault: unlocked (locks in 1m)\nrecorder: paused\nindexing: 3 pending"
        );
    }
}
