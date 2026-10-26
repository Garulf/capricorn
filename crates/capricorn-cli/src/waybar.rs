use capricorn_core::proto::{RecorderState, Status, VaultState};
use serde_json::{Value, json};

pub fn waybar(status: &Status) -> Value {
    let (alt, detail) = match status.vault {
        VaultState::Uninitialized => ("off", "not set up, run `capricorn init`".to_owned()),
        VaultState::Locked => ("locked", "vault locked".to_owned()),
        VaultState::Unlocked { locks_in_secs } => unlocked_state(status, locks_in_secs),
    };
    let text = match status.vault {
        VaultState::Uninitialized => "uninitialized",
        _ => alt,
    };
    let tooltip = with_pending(detail, status.pending_index);
    waybar_value(text, alt, &tooltip)
}

pub fn waybar_unreachable() -> Value {
    waybar_value("off", "off", "capricornd is not running")
}

fn unlocked_state(status: &Status, locks_in_secs: u64) -> (&'static str, String) {
    let locks = format!("locks in {}", format_duration(locks_in_secs));
    match status.recorder {
        Some(RecorderState::Recording) => ("recording", format!("recording, {locks}")),
        Some(RecorderState::Paused) => ("paused", format!("paused, {locks}")),
        Some(RecorderState::Idle) => ("idle", format!("idle, {locks}")),
        Some(RecorderState::Locked) => ("locked", "recorder locked".to_owned()),
        None => ("off", format!("no capture client connected, {locks}")),
    }
}

fn with_pending(detail: String, pending: u64) -> String {
    if pending == 0 {
        detail
    } else {
        format!("{detail}; indexing {pending} pending")
    }
}

fn waybar_value(text: &str, alt: &str, tooltip: &str) -> Value {
    json!({ "text": text, "alt": alt, "class": alt, "tooltip": tooltip })
}

pub fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(vault: VaultState, recorder: Option<RecorderState>) -> Status {
        Status {
            vault,
            recorder,
            pending_index: 0,
            session: false,
        }
    }

    fn alt_of(status: &Status) -> String {
        let value = waybar(status);
        assert_eq!(value["alt"], value["class"]);
        value["alt"].as_str().unwrap().to_owned()
    }

    const UNLOCKED: VaultState = VaultState::Unlocked { locks_in_secs: 840 };

    #[test]
    fn waybar_json_states() {
        let recording = Some(RecorderState::Recording);
        assert_eq!(alt_of(&status(VaultState::Uninitialized, None)), "off");
        assert_eq!(alt_of(&status(VaultState::Locked, recording)), "locked");
        assert_eq!(alt_of(&status(UNLOCKED, recording)), "recording");
        assert_eq!(
            alt_of(&status(UNLOCKED, Some(RecorderState::Paused))),
            "paused"
        );
        assert_eq!(alt_of(&status(UNLOCKED, Some(RecorderState::Idle))), "idle");
        assert_eq!(alt_of(&status(UNLOCKED, None)), "off");
        assert_eq!(waybar_unreachable()["alt"], "off");
    }

    #[test]
    fn text_names_the_state_and_tooltip_explains_it() {
        let value = waybar(&status(UNLOCKED, Some(RecorderState::Recording)));
        assert_eq!(value["text"], "recording");
        assert_eq!(value["tooltip"], "recording, locks in 14m");
        let setup = waybar(&status(VaultState::Uninitialized, None));
        assert_eq!(setup["text"], "uninitialized");
        assert!(
            setup["tooltip"]
                .as_str()
                .unwrap()
                .contains("capricorn init")
        );
    }

    #[test]
    fn pending_index_is_shown_in_tooltip() {
        let mut busy = status(VaultState::Locked, None);
        busy.pending_index = 12;
        assert!(waybar(&busy)["tooltip"].as_str().unwrap().contains("12"));
    }

    #[test]
    fn durations_are_short_and_human() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(840), "14m");
        assert_eq!(format_duration(3_900), "1h 5m");
    }
}
