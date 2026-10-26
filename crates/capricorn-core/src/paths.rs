use std::ffi::OsString;
use std::path::PathBuf;

const DEFAULT_DATA_DIR: &str = "/var/lib/capricorn";
const DEFAULT_RUNTIME_DIR: &str = "/run/capricorn";
const DEFAULT_CONFIG: &str = "/etc/capricorn/config.toml";

fn resolve(var: &str, default: &str) -> PathBuf {
    resolve_value(std::env::var_os(var), default)
}

fn resolve_value(value: Option<OsString>, default: &str) -> PathBuf {
    match value {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(default),
    }
}

pub fn data_dir() -> PathBuf {
    resolve("CAPRICORN_DATA_DIR", DEFAULT_DATA_DIR)
}

pub fn runtime_dir() -> PathBuf {
    resolve("CAPRICORN_RUNTIME_DIR", DEFAULT_RUNTIME_DIR)
}

pub fn config_path() -> PathBuf {
    resolve("CAPRICORN_CONFIG", DEFAULT_CONFIG)
}

pub fn capture_socket() -> PathBuf {
    runtime_dir().join("capture.sock")
}

/// `capture_socket` without the `CAPRICORN_RUNTIME_DIR` override.
pub fn default_capture_socket() -> PathBuf {
    PathBuf::from(DEFAULT_RUNTIME_DIR).join("capture.sock")
}

pub fn client_socket() -> PathBuf {
    runtime_dir().join("client.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins() {
        let got = resolve_value(Some("/tmp/x".into()), "/default");
        assert_eq!(got, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn default_capture_socket_is_under_run() {
        assert_eq!(
            default_capture_socket(),
            PathBuf::from("/run/capricorn/capture.sock")
        );
    }

    #[test]
    fn unset_or_empty_falls_back() {
        assert_eq!(resolve_value(None, "/default"), PathBuf::from("/default"));
        assert_eq!(
            resolve_value(Some("".into()), "/default"),
            PathBuf::from("/default")
        );
    }
}
