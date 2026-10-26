use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenSpec {
    pub class: String,
    pub workspace: String,
    pub cwd: Option<String>,
    pub url: Option<String>,
}

/// Builds the argument of `dispatch exec`, e.g. `[workspace 3] kitty --directory /x`.
///
/// Known terminals and browsers get their directory or URL flag. Any other
/// class falls back to the Exec line of a matching `.desktop` file, searched
/// in `desktop_dirs` order.
pub fn reopen_command(spec: &ReopenSpec, desktop_dirs: &[PathBuf]) -> Option<String> {
    let class = spec.class.trim();
    if class.is_empty() {
        return None;
    }
    let command =
        known_app_command(class, spec).or_else(|| desktop_file_exec(class, desktop_dirs))?;
    with_workspace_rule(&spec.workspace, &command)
}

fn with_workspace_rule(workspace: &str, command: &str) -> Option<String> {
    if workspace.is_empty() {
        return Some(command.to_owned());
    }
    let breaks_rule_syntax = |c: char| matches!(c, '[' | ']' | ';' | ',') || c.is_control();
    if workspace.contains(breaks_rule_syntax) {
        return None;
    }
    Some(format!("[workspace {workspace}] {command}"))
}

fn join_shell(args: &[String]) -> Option<String> {
    let quoted: Option<Vec<Cow<'_, str>>> = args.iter().map(|arg| quote(arg)).collect();
    Some(quoted?.join(" "))
}

fn quote(arg: &str) -> Option<Cow<'_, str>> {
    let is_plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(is_plain) {
        return Some(Cow::Borrowed(arg));
    }
    shlex::try_quote(arg).ok()
}

enum Launcher {
    Terminal {
        binary: &'static str,
        cwd_args: &'static [&'static str],
        joined_flag: bool,
    },
    Browser {
        binary: &'static str,
    },
}

fn launcher_for(class: &str) -> Option<Launcher> {
    use Launcher::{Browser, Terminal};
    let launcher = match class.to_ascii_lowercase().as_str() {
        "kitty" => Terminal {
            binary: "kitty",
            cwd_args: &["--directory"],
            joined_flag: false,
        },
        "foot" | "footclient" => Terminal {
            binary: "foot",
            cwd_args: &["-D"],
            joined_flag: false,
        },
        "alacritty" => Terminal {
            binary: "alacritty",
            cwd_args: &["--working-directory"],
            joined_flag: false,
        },
        "wezterm" | "org.wezfurlong.wezterm" => Terminal {
            binary: "wezterm",
            cwd_args: &["start", "--cwd"],
            joined_flag: false,
        },
        "ghostty" | "com.mitchellh.ghostty" => Terminal {
            binary: "ghostty",
            cwd_args: &["--working-directory"],
            joined_flag: true,
        },
        "konsole" | "org.kde.konsole" => Terminal {
            binary: "konsole",
            cwd_args: &["--workdir"],
            joined_flag: false,
        },
        "firefox" | "firefox-esr" => Browser { binary: "firefox" },
        "librewolf" => Browser {
            binary: "librewolf",
        },
        "zen" | "zen-browser" | "zen-beta" => Browser { binary: "zen" },
        "chromium" | "chromium-browser" => Browser { binary: "chromium" },
        "google-chrome" | "google-chrome-stable" => Browser {
            binary: "google-chrome",
        },
        "brave" | "brave-browser" => Browser { binary: "brave" },
        _ => return None,
    };
    Some(launcher)
}

fn known_app_command(class: &str, spec: &ReopenSpec) -> Option<String> {
    let args: Vec<String> = match launcher_for(class)? {
        Launcher::Terminal {
            binary,
            cwd_args,
            joined_flag,
        } => {
            let mut args = vec![binary.to_owned()];
            if let Some(cwd) = &spec.cwd {
                if joined_flag {
                    args.push(format!("{}={cwd}", cwd_args[0]));
                } else {
                    args.extend(cwd_args.iter().map(|arg| (*arg).to_owned()));
                    args.push(cwd.clone());
                }
            }
            args
        }
        Launcher::Browser { binary } => {
            let mut args = vec![binary.to_owned()];
            if let Some(url) = &spec.url {
                args.extend(["--new-window".to_owned(), url.clone()]);
            }
            args
        }
    };
    join_shell(&args)
}

fn desktop_file_exec(class: &str, dirs: &[PathBuf]) -> Option<String> {
    dirs.iter()
        .filter_map(|dir| find_in_dir(dir, class))
        .find_map(|entry| exec_without_field_codes(&entry.exec))
}

struct DesktopEntry {
    exec: String,
}

fn find_in_dir(dir: &Path, class: &str) -> Option<DesktopEntry> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| read_matching_entry(path, class))
        .next()
}

fn read_matching_entry(path: &Path, class: &str) -> Option<DesktopEntry> {
    let text = fs::read_to_string(path).ok()?;
    let fields = desktop_entry_group(&text);
    let stem_matches = path
        .file_stem()
        .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case(class));
    let wm_class_matches = fields
        .startup_wm_class
        .is_some_and(|wm_class| wm_class.eq_ignore_ascii_case(class));
    if !(stem_matches || wm_class_matches) {
        return None;
    }
    Some(DesktopEntry {
        exec: fields.exec?.to_owned(),
    })
}

#[derive(Default)]
struct DesktopFields<'a> {
    exec: Option<&'a str>,
    startup_wm_class: Option<&'a str>,
}

fn desktop_entry_group(text: &str) -> DesktopFields<'_> {
    let mut fields = DesktopFields::default();
    let mut in_main_group = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
        } else if in_main_group {
            if let Some(value) = line.strip_prefix("Exec=") {
                fields.exec.get_or_insert(value.trim());
            } else if let Some(value) = line.strip_prefix("StartupWMClass=") {
                fields.startup_wm_class.get_or_insert(value.trim());
            }
        }
    }
    fields
}

const FIELD_CODES: &str = "fFuUdDnNickvm";

fn exec_without_field_codes(exec: &str) -> Option<String> {
    let tokens = shlex::split(exec)?;
    let cleaned: Vec<String> = tokens
        .iter()
        .filter(|token| !is_lone_field_code(token))
        .map(|token| strip_embedded_codes(token))
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    join_shell(&cleaned)
}

fn is_lone_field_code(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next() == Some('%')
        && chars.next().is_some_and(|code| FIELD_CODES.contains(code))
        && chars.next().is_none()
}

fn strip_embedded_codes(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                out.push('%');
                chars.next();
            }
            Some(code) if FIELD_CODES.contains(*code) => {
                chars.next();
            }
            _ => out.push('%'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn spec(class: &str, cwd: Option<&str>, url: Option<&str>) -> ReopenSpec {
        ReopenSpec {
            class: class.into(),
            workspace: "3".into(),
            cwd: cwd.map(Into::into),
            url: url.map(Into::into),
        }
    }

    fn reopen(spec: &ReopenSpec) -> Option<String> {
        reopen_command(spec, &[])
    }

    #[test]
    fn reopen_kitty_cwd() {
        assert_eq!(
            reopen(&spec("kitty", Some("/home/u/proj"), None)).as_deref(),
            Some("[workspace 3] kitty --directory /home/u/proj")
        );
    }

    #[test]
    fn reopen_terminals_use_their_cwd_flag() {
        let cases = [
            ("foot", "foot -D /w"),
            ("Alacritty", "alacritty --working-directory /w"),
            ("org.wezfurlong.wezterm", "wezterm start --cwd /w"),
            ("com.mitchellh.ghostty", "ghostty --working-directory=/w"),
            ("org.kde.konsole", "konsole --workdir /w"),
        ];
        for (class, expected) in cases {
            assert_eq!(
                reopen(&spec(class, Some("/w"), None)),
                Some(format!("[workspace 3] {expected}")),
                "{class}"
            );
        }
    }

    #[test]
    fn reopen_terminal_without_cwd_is_bare() {
        assert_eq!(
            reopen(&spec("kitty", None, None)).as_deref(),
            Some("[workspace 3] kitty")
        );
    }

    #[test]
    fn reopen_firefox_url() {
        assert_eq!(
            reopen(&spec(
                "firefox",
                None,
                Some("https://example.com/a?b=1&c=2")
            ))
            .as_deref(),
            Some("[workspace 3] firefox --new-window 'https://example.com/a?b=1&c=2'")
        );
    }

    #[test]
    fn reopen_browsers_use_new_window() {
        let cases = [
            ("librewolf", "librewolf"),
            ("zen", "zen"),
            ("Chromium", "chromium"),
            ("google-chrome", "google-chrome"),
            ("Brave-browser", "brave"),
        ];
        for (class, binary) in cases {
            assert_eq!(
                reopen(&spec(class, None, Some("https://x.org"))),
                Some(format!("[workspace 3] {binary} --new-window https://x.org")),
                "{class}"
            );
        }
    }

    #[test]
    fn reopen_browser_without_url_is_bare() {
        assert_eq!(
            reopen(&spec("firefox", None, None)).as_deref(),
            Some("[workspace 3] firefox")
        );
    }

    #[test]
    fn reopen_quotes_spaces() {
        assert_eq!(
            reopen(&spec("kitty", Some("/home/u/my project"), None)).as_deref(),
            Some("[workspace 3] kitty --directory '/home/u/my project'")
        );
    }

    #[test]
    fn reopen_quotes_shell_metacharacters() {
        let command = reopen(&spec("kitty", Some("/a;rm -rf $HOME"), None)).unwrap();
        assert_eq!(command, "[workspace 3] kitty --directory '/a;rm -rf $HOME'");
    }

    #[test]
    fn reopen_unknown_none() {
        assert_eq!(reopen(&spec("no-such-app", None, None)), None);
        assert_eq!(reopen(&spec("", None, None)), None);
    }

    #[test]
    fn reopen_rejects_workspace_that_could_inject_rules() {
        let mut hostile = spec("kitty", None, None);
        hostile.workspace = "1] ; float [x".into();
        assert_eq!(reopen(&hostile), None);
    }

    #[test]
    fn reopen_empty_workspace_omits_rule() {
        let mut no_ws = spec("kitty", None, None);
        no_ws.workspace = String::new();
        assert_eq!(reopen(&no_ws).as_deref(), Some("kitty"));
    }

    fn write_desktop(dir: &std::path::Path, file: &str, body: &str) {
        fs::write(
            dir.join(file),
            format!("[Desktop Entry]\nType=Application\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn reopen_desktop_file_fallback() {
        let dir = tempfile::tempdir().unwrap();
        write_desktop(dir.path(), "foo.desktop", "Name=Foo\nExec=foo %U");
        write_desktop(dir.path(), "other.desktop", "Name=Other\nExec=other");
        let command = reopen_command(&spec("Foo", None, None), &[dir.path().into()]);
        assert_eq!(command.as_deref(), Some("[workspace 3] foo"));
    }

    #[test]
    fn reopen_desktop_file_matches_startup_wm_class() {
        let dir = tempfile::tempdir().unwrap();
        write_desktop(
            dir.path(),
            "org.example.Viewer.desktop",
            "StartupWMClass=ImageViewer\nExec=/usr/bin/viewer --flag \"two words\" %f",
        );
        let command = reopen_command(&spec("imageviewer", None, None), &[dir.path().into()]);
        assert_eq!(
            command.as_deref(),
            Some("[workspace 3] /usr/bin/viewer --flag 'two words'")
        );
    }

    #[test]
    fn reopen_desktop_file_strips_all_field_codes() {
        let dir = tempfile::tempdir().unwrap();
        write_desktop(
            dir.path(),
            "app.desktop",
            "Exec=env FOO=1 app %f %F %u %U %i %c %k --file=%f 100%%",
        );
        let command = reopen_command(&spec("app", None, None), &[dir.path().into()]);
        assert_eq!(
            command.as_deref(),
            Some("[workspace 3] env FOO=1 app --file= 100%")
        );
    }

    #[test]
    fn reopen_desktop_file_earlier_dir_wins_and_missing_dirs_are_skipped() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_desktop(first.path(), "app.desktop", "Exec=first");
        write_desktop(second.path(), "app.desktop", "Exec=second");
        let dirs = [
            PathBuf::from("/nonexistent/applications"),
            first.path().into(),
            second.path().into(),
        ];
        assert_eq!(
            reopen_command(&spec("app", None, None), &dirs).as_deref(),
            Some("[workspace 3] first")
        );
    }

    #[test]
    fn reopen_desktop_file_without_exec_is_none() {
        let dir = tempfile::tempdir().unwrap();
        write_desktop(dir.path(), "app.desktop", "Name=NoExec");
        assert_eq!(
            reopen_command(&spec("app", None, None), &[dir.path().into()]),
            None
        );
    }

    #[test]
    fn reopen_desktop_file_ignores_exec_in_other_groups() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("app.desktop"),
            "[Desktop Entry]\nExec=real\n\n[Desktop Action new]\nExec=action\n",
        )
        .unwrap();
        assert_eq!(
            reopen_command(&spec("app", None, None), &[dir.path().into()]).as_deref(),
            Some("[workspace 3] real")
        );
    }
}
