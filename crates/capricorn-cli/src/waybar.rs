use capricorn_core::proto::{RecorderState, Status, VaultState};
use serde_json::{Value, json};

/// Recording goes on while the vault is locked, so the module shows the recorder and
/// leaves the vault to the tooltip.
pub fn waybar(status: &Status) -> Value {
    if status.vault == VaultState::Uninitialized {
        let tooltip = with_pending(
            "not set up, run `capricorn init`".to_owned(),
            status.pending_index,
        );
        return waybar_value("uninitialized", "off", &tooltip);
    }
    let (alt, recorder) = recorder_state(status.recorder);
    let detail = format!("{recorder}, {}", vault_detail(status.vault));
    waybar_value(alt, alt, &with_pending(detail, status.pending_index))
}

pub fn waybar_unreachable() -> Value {
    waybar_value("off", "off", "capricornd is not running")
}

fn recorder_state(recorder: Option<RecorderState>) -> (&'static str, &'static str) {
    match recorder {
        Some(RecorderState::Recording) => ("recording", "recording"),
        Some(RecorderState::Paused) => ("paused", "paused"),
        Some(RecorderState::Idle) => ("idle", "idle"),
        Some(RecorderState::Locked) => ("locked", "screen locked"),
        None => ("off", "no capture client connected"),
    }
}

fn vault_detail(vault: VaultState) -> String {
    match vault {
        VaultState::Unlocked { locks_in_secs } => {
            format!("vault locks in {}", format_duration(locks_in_secs))
        }
        _ => "vault locked".to_owned(),
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
        for vault in [VaultState::Locked, UNLOCKED] {
            for (recorder, class) in [
                (Some(RecorderState::Recording), "recording"),
                (Some(RecorderState::Paused), "paused"),
                (Some(RecorderState::Idle), "idle"),
                (Some(RecorderState::Locked), "locked"),
                (None, "off"),
            ] {
                assert_eq!(
                    alt_of(&status(vault, recorder)),
                    class,
                    "{vault:?} {recorder:?}"
                );
            }
        }
        let recording = Some(RecorderState::Recording);
        assert_eq!(alt_of(&status(VaultState::Uninitialized, recording)), "off");
        assert_eq!(waybar_unreachable()["alt"], "off");
    }

    #[test]
    fn text_names_the_recorder_and_tooltip_adds_the_vault() {
        let value = waybar(&status(UNLOCKED, Some(RecorderState::Recording)));
        assert_eq!(value["text"], "recording");
        assert_eq!(value["tooltip"], "recording, vault locks in 14m");
        let locked = waybar(&status(VaultState::Locked, Some(RecorderState::Recording)));
        assert_eq!(locked["text"], "recording");
        assert_eq!(locked["tooltip"], "recording, vault locked");
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
