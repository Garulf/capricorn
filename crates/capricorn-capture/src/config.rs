use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use capricorn_core::rules::{Rule, RuleSet};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// On-disk shape of `capture.toml`, written by the Home Manager module.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CaptureFile {
    exclude: Vec<RuleFile>,
    pause_fullscreen_classes: Vec<String>,
    floor_secs: u64,
    settle_ms: u64,
    idle_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    class: Option<String>,
    title: Option<String>,
    workspace: Option<String>,
}

impl Default for CaptureFile {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            pause_fullscreen_classes: ["steam_app_.*", "gamescope", "mpv"]
                .map(String::from)
                .into(),
            floor_secs: 5,
            settle_ms: 1500,
            idle_secs: 300,
        }
    }
}

#[derive(Debug)]
pub struct Settings {
    pub rules: RuleSet,
    pub pause_fullscreen: Vec<Regex>,
    pub floor: Duration,
    pub settle: Duration,
    pub idle: Duration,
}

pub fn default_path() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config_home.join("capricorn").join("capture.toml"))
}

/// A missing file yields the defaults.
pub fn load(path: &Path) -> anyhow::Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("loading {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => parse(""),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// User rules are added to `RuleSet::defaults()`. Their `class` and `title` are
/// case-insensitive searches; `pause_fullscreen_classes` must match the whole class.
pub fn parse(text: &str) -> anyhow::Result<Settings> {
    let file: CaptureFile = toml::from_str(text)?;
    if file.floor_secs == 0 || file.settle_ms == 0 || file.idle_secs == 0 {
        bail!("floor_secs, settle_ms and idle_secs must be positive");
    }
    let mut rules = RuleSet::defaults();
    for rule in &file.exclude {
        rules.rules.push(rule.compile()?);
    }
    let pause_fullscreen = file
        .pause_fullscreen_classes
        .iter()
        .map(|class| full_match(class))
        .collect::<anyhow::Result<_>>()?;
    Ok(Settings {
        rules,
        pause_fullscreen,
        floor: Duration::from_secs(file.floor_secs),
        settle: Duration::from_millis(file.settle_ms),
        idle: Duration::from_secs(file.idle_secs),
    })
}

impl RuleFile {
    fn compile(&self) -> anyhow::Result<Rule> {
        Ok(Rule {
            class: self.class.as_deref().map(case_insensitive).transpose()?,
            title: self.title.as_deref().map(case_insensitive).transpose()?,
            workspace: self.workspace.clone(),
        })
    }
}

fn case_insensitive(pattern: &str) -> anyhow::Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("invalid exclude pattern `{pattern}`"))
}

fn full_match(pattern: &str) -> anyhow::Result<Regex> {
    Regex::new(&format!("^(?:{pattern})$"))
        .with_context(|| format!("invalid pause_fullscreen_classes pattern `{pattern}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_spec_defaults() {
        let settings = parse("").unwrap();
        assert_eq!(settings.floor, Duration::from_secs(5));
        assert_eq!(settings.settle, Duration::from_millis(1500));
        assert_eq!(settings.idle, Duration::from_secs(300));
        assert!(settings.rules.matches("org.keepassxc.KeePassXC", "", "1"));
        let pauses = |class: &str| {
            settings
                .pause_fullscreen
                .iter()
                .any(|re| re.is_match(class))
        };
        assert!(pauses("steam_app_570"));
        assert!(pauses("gamescope"));
        assert!(pauses("mpv"));
        assert!(!pauses("mpvpaper"));
        assert!(!pauses("firefox"));
    }

    #[test]
    fn user_rules_merge_with_defaults() {
        let settings = parse(
            r#"
            [[exclude]]
            class = "^signal$"

            [[exclude]]
            workspace = "9"

            [[exclude]]
            class = "firefox"
            title = "bank"
            "#,
        )
        .unwrap();
        let rules = &settings.rules;
        assert!(rules.matches("Signal", "chat", "1"));
        assert!(rules.matches("kitty", "x", "9"));
        assert!(rules.matches("firefox", "My Bank - Login", "1"));
        assert!(!rules.matches("firefox", "News", "1"));
        assert!(rules.matches("Bitwarden", "", "1"));
    }

    #[test]
    fn pause_classes_replace_defaults_and_full_match() {
        let settings = parse(r#"pause_fullscreen_classes = ["vlc"]"#).unwrap();
        let pauses = |class: &str| {
            settings
                .pause_fullscreen
                .iter()
                .any(|re| re.is_match(class))
        };
        assert!(pauses("vlc"));
        assert!(!pauses("mpv"));
        assert!(!pauses("vlc2"));
    }

    #[test]
    fn timing_overrides() {
        let settings = parse("floor_secs = 10\nsettle_ms = 500\nidle_secs = 60").unwrap();
        assert_eq!(settings.floor, Duration::from_secs(10));
        assert_eq!(settings.settle, Duration::from_millis(500));
        assert_eq!(settings.idle, Duration::from_secs(60));
    }

    #[test]
    fn invalid_files_are_errors() {
        assert!(parse("[[exclude]]\nclass = \"(\"").is_err());
        assert!(parse("pause_fullscreen_classes = [\"[\"]").is_err());
        assert!(parse("unknown = 1").is_err());
        assert!(parse("[[exclude]]\nklass = \"x\"").is_err());
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(&dir.path().join("capture.toml")).unwrap();
        assert_eq!(settings.floor, Duration::from_secs(5));
    }
}
