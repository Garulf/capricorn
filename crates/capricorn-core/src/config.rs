use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfOverride {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub lock_timeout_secs: u64,
    /// `0` in the file means no age limit.
    #[serde(with = "zero_is_unlimited")]
    pub retention_days: Option<u32>,
    /// `0` in the file means no size limit.
    #[serde(with = "zero_is_unlimited")]
    pub retention_max_gib: Option<u64>,
    pub clipboard_retention_days: Option<u32>,
    pub forget_grace_mins: u32,
    /// Uids allowed on the daemon sockets. Empty is refused unless `allow_any_uid`.
    pub allowed_uids: Vec<u32>,
    /// Lets an empty `allowed_uids` admit every uid; for development and tests only.
    pub allow_any_uid: bool,
    pub capture_clients: Vec<PathBuf>,
    pub ui_clients: Vec<PathBuf>,
    pub secret_patterns: Vec<String>,
    pub kdf: Option<KdfOverride>,
    /// Overrides `paths::data_dir()` when set.
    pub data_dir: Option<PathBuf>,
    /// Overrides `paths::runtime_dir()` when set.
    pub runtime_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lock_timeout_secs: 15 * 60,
            retention_days: Some(90),
            retention_max_gib: Some(50),
            clipboard_retention_days: None,
            forget_grace_mins: 15,
            allowed_uids: Vec::new(),
            allow_any_uid: false,
            capture_clients: Vec::new(),
            ui_clients: Vec::new(),
            secret_patterns: Vec::new(),
            kdf: None,
            data_dir: None,
            runtime_dir: None,
        }
    }
}

impl Config {
    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(crate::paths::data_dir)
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.runtime_dir
            .clone()
            .unwrap_or_else(crate::paths::runtime_dir)
    }

    /// Compiles `secret_patterns`, failing on the first invalid regex.
    pub fn compiled_secret_patterns(&self) -> anyhow::Result<Vec<Regex>> {
        self.secret_patterns
            .iter()
            .map(|p| Regex::new(p).with_context(|| format!("invalid secret pattern `{p}`")))
            .collect()
    }
}

/// TOML has no null, so an unlimited retention bound is written as `0`.
mod zero_is_unlimited {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Copy + Default + Serialize,
        S: Serializer,
    {
        value.unwrap_or_default().serialize(serializer)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        T: Deserialize<'de> + Default + PartialEq,
        D: Deserializer<'de>,
    {
        let value = T::deserialize(deserializer)?;
        Ok((value != T::default()).then_some(value))
    }
}

/// A missing file yields `Config::default()`; any other read or parse failure is an error.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.lock_timeout_secs, 900);
        assert_eq!(cfg.retention_days, Some(90));
        assert_eq!(cfg.retention_max_gib, Some(50));
        assert_eq!(cfg.forget_grace_mins, 15);
        assert!(cfg.kdf.is_none());
        assert!(cfg.data_dir.is_none() && cfg.runtime_dir.is_none());
        assert!(cfg.allowed_uids.is_empty() && !cfg.allow_any_uid);
    }

    #[test]
    fn explicit_dirs_win_over_env_defaults() {
        let cfg: Config =
            toml::from_str("data_dir = \"/tmp/d\"\nruntime_dir = \"/tmp/r\"\n").unwrap();
        assert_eq!(cfg.data_dir(), PathBuf::from("/tmp/d"));
        assert_eq!(cfg.runtime_dir(), PathBuf::from("/tmp/r"));
    }

    #[test]
    fn config_partial_override_keeps_other_defaults() {
        let cfg: Config = toml::from_str(
            "lock_timeout_secs = 60\nretention_days = 30\n[kdf]\nmemory_kib = 8\niterations = 1\nparallelism = 1\n",
        )
        .unwrap();
        assert_eq!(cfg.lock_timeout_secs, 60);
        assert_eq!(cfg.retention_days, Some(30));
        assert_eq!(cfg.retention_max_gib, Some(50));
        assert_eq!(cfg.kdf.unwrap().memory_kib, 8);
    }

    #[test]
    fn zero_retention_means_unlimited() {
        let cfg: Config = toml::from_str("retention_days = 0\nretention_max_gib = 0\n").unwrap();
        assert_eq!(cfg.retention_days, None);
        assert_eq!(cfg.retention_max_gib, None);
        let round: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(round, cfg);
    }

    #[test]
    fn config_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn config_invalid_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "lock_timeout_secs = \"soon\"").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn invalid_secret_pattern_is_error() {
        let cfg = Config {
            secret_patterns: vec!["(".into()],
            ..Config::default()
        };
        assert!(cfg.compiled_secret_patterns().is_err());
    }
}
