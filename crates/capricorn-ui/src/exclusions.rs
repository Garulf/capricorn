use std::path::{Path, PathBuf};

use serde::Deserialize;

const ALWAYS_EXCLUDED: [&str; 2] = ["Password managers", "Private browser windows"];

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CaptureFile {
    exclude: Vec<RuleFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RuleFile {
    class: Option<String>,
    title: Option<String>,
    workspace: Option<String>,
}

impl RuleFile {
    fn describe(&self) -> Option<String> {
        let parts: Vec<String> = [
            self.class.as_ref().map(|c| format!("app {c}")),
            self.title.as_ref().map(|t| format!("title {t}")),
            self.workspace.as_ref().map(|w| format!("workspace {w}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Same file `capricorn-capture` reads; written by the Home Manager module.
pub fn capture_config_path() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config_home.join("capricorn").join("capture.toml"))
}

/// Human-readable list for the timeline's "Never recorded" section.
pub fn never_recorded(path: Option<&Path>) -> Vec<String> {
    let text = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    describe(&text)
}

fn describe(capture_toml: &str) -> Vec<String> {
    let file: CaptureFile = toml::from_str(capture_toml).unwrap_or_default();
    ALWAYS_EXCLUDED
        .iter()
        .map(|s| s.to_string())
        .chain(file.exclude.iter().filter_map(RuleFile::describe))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_then_user_rules() {
        let got = describe(
            "floor_secs = 5\n[[exclude]]\nclass = \"signal\"\n[[exclude]]\nworkspace = \"9\"\ntitle = \"bank\"\n",
        );
        assert_eq!(
            got,
            [
                "Password managers",
                "Private browser windows",
                "app signal",
                "title bank, workspace 9"
            ]
        );
    }

    #[test]
    fn unreadable_config_still_lists_builtins() {
        assert_eq!(describe("exclude = 3").len(), 2);
        assert_eq!(never_recorded(None).len(), 2);
    }
}
