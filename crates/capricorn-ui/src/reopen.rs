use std::ffi::OsString;
use std::path::PathBuf;

use capricorn_core::proto::RecordDetail;
use capricorn_hypr::{Hypr, ReopenSpec, reopen_plan};

const DEFAULT_DATA_DIRS: &str = "/usr/local/share:/usr/share";

pub fn spec_for(detail: &RecordDetail) -> ReopenSpec {
    ReopenSpec {
        class: detail.hit.app.clone(),
        workspace: detail.hit.workspace.clone(),
        cwd: detail.cwd.clone(),
        url: detail.url.clone(),
    }
}

/// `applications/` under the XDG data home first, then each XDG data dir.
fn desktop_dirs(
    data_home: Option<OsString>,
    home: Option<OsString>,
    data_dirs: Option<OsString>,
) -> Vec<PathBuf> {
    let data_home = data_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".local/share")));
    let data_dirs = data_dirs
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_DATA_DIRS.into());
    data_home
        .into_iter()
        .chain(std::env::split_paths(&data_dirs))
        .map(|dir| dir.join("applications"))
        .collect()
}

fn system_desktop_dirs() -> Vec<PathBuf> {
    desktop_dirs(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_DATA_DIRS"),
    )
}

/// Best effort: a missing launcher or Hyprland is reported, never retried.
pub async fn reopen(detail: RecordDetail) -> Result<(), String> {
    let spec = spec_for(&detail);
    let plan = reopen_plan(&spec, &system_desktop_dirs())
        .ok_or_else(|| format!("no way to launch {}", spec.class))?;
    let hypr = Hypr::from_env().map_err(|e| e.to_string())?;
    hypr.reopen(&plan).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_dirs_follow_xdg_order() {
        let dirs = desktop_dirs(
            None,
            Some("/home/u".into()),
            Some("/run/current-system/sw/share:/etc/profiles/per-user/u/share".into()),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/home/u/.local/share/applications"),
                PathBuf::from("/run/current-system/sw/share/applications"),
                PathBuf::from("/etc/profiles/per-user/u/share/applications"),
            ]
        );
    }

    #[test]
    fn desktop_dirs_defaults() {
        let dirs = desktop_dirs(Some("/x".into()), None, None);
        assert_eq!(
            dirs,
            [
                PathBuf::from("/x/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
    }
}
