use std::time::{Duration, Instant};

use capricorn_core::rules::RuleSet;

use crate::exclude::{Snapshot, focus_is_excluded};

/// A copy is dropped when an excluded window had focus this long before it was seen. The
/// selection can change hands after the user already moved focus away from the source.
pub const CLIPBOARD_LOOKBACK: Duration = Duration::from_secs(2);
const KEEP: Duration = Duration::from_secs(10);

/// When focus moved onto or off excluded windows, as far as Hyprland told us. Marks can
/// arrive out of order: events carry the time they were read, snapshots the time they
/// were taken.
#[derive(Debug, Default)]
pub struct FocusHistory {
    marks: Vec<Mark>,
}

#[derive(Debug, Clone, Copy)]
struct Mark {
    at: Instant,
    excluded: bool,
}

impl FocusHistory {
    pub fn record(&mut self, at: Instant, excluded: bool) {
        let index = self.marks.partition_point(|m| m.at <= at);
        self.marks.insert(index, Mark { at, excluded });
        self.prune(at);
    }

    pub fn record_snapshot(&mut self, snap: &Snapshot, rules: &RuleSet, at: Instant) {
        self.record(at, focus_is_excluded(snap, rules));
    }

    /// True when an excluded window may have had focus at any point from
    /// `CLIPBOARD_LOOKBACK` before `seen` up to the latest mark.
    pub fn excluded_since_lookback(&self, seen: Instant) -> bool {
        let from = seen.checked_sub(CLIPBOARD_LOOKBACK).unwrap_or(seen);
        let in_effect_at_start = self.marks.iter().rev().find(|m| m.at < from);
        in_effect_at_start.is_some_and(|m| m.excluded)
            || self.marks.iter().any(|m| m.at >= from && m.excluded)
    }

    /// Keeps the newest mark older than `KEEP`, which still says what focus was then.
    fn prune(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(KEEP) else {
            return;
        };
        let older = self.marks.partition_point(|m| m.at < cutoff);
        if older > 1 {
            self.marks.drain(..older - 1);
        }
    }
}

/// An `activewindow` event names only class and title. The window is on a shown
/// workspace, so it is treated as excluded when a rule matches on any of them.
pub fn event_window_excluded(
    class: &str,
    title: &str,
    snap: Option<&Snapshot>,
    rules: &RuleSet,
) -> bool {
    let shown = snap.into_iter().flat_map(|s| &s.monitors).flat_map(|m| {
        [&m.active_workspace, &m.special_workspace]
            .into_iter()
            .filter(|ws| !ws.name.is_empty())
            .map(|ws| ws.name.as_str())
    });
    std::iter::once("")
        .chain(shown)
        .any(|workspace| rules.matches(class, title, workspace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_core::rules::Rule;
    use capricorn_hypr::{Monitor, WsRef};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn excluded_focus_within_lookback_drops() {
        let t = Instant::now() + KEEP;
        let mut history = FocusHistory::default();
        history.record(t, false);
        history.record(t + ms(1000), true);
        history.record(t + ms(1200), false);
        assert!(history.excluded_since_lookback(t + ms(3000)));
        assert!(history.excluded_since_lookback(t + ms(3199)));
        assert!(!history.excluded_since_lookback(t + ms(3201)));
    }

    #[test]
    fn focus_still_excluded_from_before_lookback_drops() {
        let t = Instant::now() + KEEP;
        let mut history = FocusHistory::default();
        history.record(t, true);
        assert!(history.excluded_since_lookback(t + ms(5000)));
        history.record(t + ms(5500), false);
        assert!(history.excluded_since_lookback(t + ms(5000)));
        assert!(!history.excluded_since_lookback(t + ms(7600)));
    }

    #[test]
    fn excluded_focus_after_the_copy_drops() {
        let t = Instant::now() + KEEP;
        let mut history = FocusHistory::default();
        history.record(t, false);
        history.record(t + ms(4000), true);
        assert!(history.excluded_since_lookback(t + ms(3500)));
    }

    #[test]
    fn late_events_are_placed_in_time_order() {
        let t = Instant::now() + KEEP;
        let mut history = FocusHistory::default();
        history.record(t + ms(3000), false);
        history.record(t, true);
        assert!(!history.excluded_since_lookback(t + ms(5500)));
        assert!(history.excluded_since_lookback(t + ms(4500)));
    }

    #[test]
    fn empty_history_does_not_drop() {
        assert!(!FocusHistory::default().excluded_since_lookback(Instant::now()));
    }

    #[test]
    fn pruning_keeps_the_state_in_effect() {
        let t = Instant::now();
        let mut history = FocusHistory::default();
        history.record(t, true);
        for i in 1..=100 {
            history.record(t + ms(i * 10), true);
        }
        history.record(t + KEEP * 3, true);
        assert!(history.marks.len() <= 3);
        assert!(history.excluded_since_lookback(t + KEEP * 3));
    }

    #[test]
    fn event_window_matches_rules_on_shown_workspaces() {
        let mut rules = RuleSet::defaults();
        rules.rules.push(Rule {
            class: None,
            title: None,
            workspace: Some("9".into()),
        });
        let snap = Snapshot {
            monitors: vec![Monitor {
                active_workspace: WsRef {
                    id: 9,
                    name: "9".into(),
                },
                ..Monitor::default()
            }],
            ..Snapshot::default()
        };
        assert!(event_window_excluded("kitty", "~", Some(&snap), &rules));
        assert!(!event_window_excluded("kitty", "~", None, &rules));
        assert!(event_window_excluded("KeePassXC", "db", None, &rules));
        assert!(event_window_excluded(
            "firefox",
            "Mozilla Firefox Private Browsing",
            None,
            &rules
        ));
    }
}
