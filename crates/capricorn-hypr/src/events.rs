#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ActiveWindow { class: String, title: String },
    Workspace(String),
    FocusedMon { mon: String, ws: String },
    OpenWindow,
    CloseWindow,
    MoveWindow,
    WindowTitle,
    Fullscreen(bool),
    Other,
}

/// Parses one `EVENT>>DATA` line from socket2.
///
/// Only the unversioned events are mapped; their `v2` twins are emitted for
/// the same occurrence and would double every event.
pub fn parse_event(line: &str) -> Event {
    let line = line.trim_end_matches(['\n', '\r']);
    let Some((name, data)) = line.split_once(">>") else {
        return Event::Other;
    };
    match name {
        "activewindow" => {
            let (class, title) = data.split_once(',').unwrap_or((data, ""));
            Event::ActiveWindow {
                class: class.into(),
                title: title.into(),
            }
        }
        "workspace" => Event::Workspace(data.into()),
        "focusedmon" => {
            let (mon, ws) = data.split_once(',').unwrap_or((data, ""));
            Event::FocusedMon {
                mon: mon.into(),
                ws: ws.into(),
            }
        }
        "fullscreen" => Event::Fullscreen(data == "1"),
        "openwindow" => Event::OpenWindow,
        "closewindow" => Event::CloseWindow,
        "movewindow" => Event::MoveWindow,
        "windowtitle" => Event::WindowTitle,
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(class: &str, title: &str) -> Event {
        Event::ActiveWindow {
            class: class.into(),
            title: title.into(),
        }
    }

    #[test]
    fn parse_activewindow() {
        assert_eq!(
            parse_event("activewindow>>kitty,~/x"),
            active("kitty", "~/x")
        );
    }

    #[test]
    fn parse_activewindow_title_with_commas_and_arrows() {
        assert_eq!(
            parse_event("activewindow>>firefox,a, b >> c, d"),
            active("firefox", "a, b >> c, d")
        );
    }

    #[test]
    fn parse_activewindow_empty_when_nothing_focused() {
        assert_eq!(parse_event("activewindow>>,"), active("", ""));
        assert_eq!(parse_event("activewindow>>"), active("", ""));
    }

    #[test]
    fn parse_workspace() {
        assert_eq!(parse_event("workspace>>3"), Event::Workspace("3".into()));
    }

    #[test]
    fn parse_focusedmon() {
        assert_eq!(
            parse_event("focusedmon>>DP-1,2"),
            Event::FocusedMon {
                mon: "DP-1".into(),
                ws: "2".into()
            }
        );
    }

    #[test]
    fn parse_fullscreen() {
        assert_eq!(parse_event("fullscreen>>1"), Event::Fullscreen(true));
        assert_eq!(parse_event("fullscreen>>0"), Event::Fullscreen(false));
    }

    #[test]
    fn parse_window_lifecycle_events() {
        assert_eq!(
            parse_event("openwindow>>80a6f50,2,kitty,kitty"),
            Event::OpenWindow
        );
        assert_eq!(parse_event("closewindow>>80a6f50"), Event::CloseWindow);
        assert_eq!(parse_event("movewindow>>80a6f50,3"), Event::MoveWindow);
        assert_eq!(parse_event("windowtitle>>80a6f50"), Event::WindowTitle);
    }

    #[test]
    fn versioned_duplicates_are_other() {
        assert_eq!(parse_event("workspacev2>>3,3"), Event::Other);
        assert_eq!(parse_event("activewindowv2>>80a6f50"), Event::Other);
        assert_eq!(parse_event("windowtitlev2>>80a6f50,t"), Event::Other);
    }

    #[test]
    fn parse_unknown_and_malformed_are_other() {
        assert_eq!(parse_event("monitoradded>>DP-2"), Event::Other);
        assert_eq!(parse_event("no separator here"), Event::Other);
        assert_eq!(parse_event(""), Event::Other);
    }

    #[test]
    fn parse_tolerates_trailing_newline() {
        assert_eq!(parse_event("workspace>>3\n"), Event::Workspace("3".into()));
        assert_eq!(
            parse_event("workspace>>3\r\n"),
            Event::Workspace("3".into())
        );
    }
}
