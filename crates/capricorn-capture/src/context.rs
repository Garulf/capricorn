use capricorn_core::proto::WindowContext;
use capricorn_hypr::{Client, Monitor};
use regex::Regex;

use crate::exclude::Snapshot;

const TERMINAL_CLASSES: &[&str] = &[
    "kitty",
    "foot",
    "footclient",
    "alacritty",
    "wezterm",
    "ghostty",
    "konsole",
    "org.wezfurlong.wezterm",
    "com.mitchellh.ghostty",
];

pub fn is_terminal(class: &str) -> bool {
    TERMINAL_CLASSES
        .iter()
        .any(|terminal| terminal.eq_ignore_ascii_case(class))
}

/// Context for a frame of `mon`: the focused window when it is on `mon`, otherwise the
/// largest visible window there. `cwd` is left for the caller to fill.
pub fn window_context(snap: &Snapshot, mon: &Monitor) -> WindowContext {
    let focused_here = snap.active.clone().filter(|c| c.monitor == mon.id);
    let shown = focused_here.or_else(|| {
        snap.visible_clients()
            .into_iter()
            .filter(|c| c.monitor == mon.id)
            .max_by_key(area)
    });
    match shown {
        Some(client) => client_context(&client, &mon.name),
        None => empty_context(mon),
    }
}

/// Context for something that happened in the focused window, such as a clipboard copy.
pub fn focused_context(snap: &Snapshot) -> WindowContext {
    let monitor_of = |id: i32| snap.monitors.iter().find(|m| m.id == id);
    match &snap.active {
        Some(client) => {
            let output = monitor_of(client.monitor).map_or("", |m| m.name.as_str());
            client_context(client, output)
        }
        None => snap
            .monitors
            .iter()
            .find(|m| m.focused)
            .map(empty_context)
            .unwrap_or_default(),
    }
}

/// True when a visible fullscreen window belongs to a class that pauses recording.
pub fn fullscreen_paused(snap: &Snapshot, pause_classes: &[Regex]) -> bool {
    snap.visible_clients()
        .iter()
        .any(|c| c.fullscreen != 0 && pause_classes.iter().any(|re| re.is_match(&c.class)))
}

fn area(client: &Client) -> i64 {
    i64::from(client.size[0]) * i64::from(client.size[1])
}

fn client_context(client: &Client, output: &str) -> WindowContext {
    WindowContext {
        class: client.class.clone(),
        title: client.title.clone(),
        workspace: client.workspace.name.clone(),
        output: output.to_owned(),
        pid: u32::try_from(client.pid).ok().filter(|&pid| pid > 0),
        cwd: None,
    }
}

fn empty_context(mon: &Monitor) -> WindowContext {
    WindowContext {
        workspace: mon.active_workspace.name.clone(),
        output: mon.name.clone(),
        ..WindowContext::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_hypr::WsRef;

    fn ws(id: i32) -> WsRef {
        WsRef {
            id,
            name: id.to_string(),
        }
    }

    fn monitor(id: i32, name: &str, workspace: i32, focused: bool) -> Monitor {
        Monitor {
            id,
            name: name.into(),
            width: 100,
            height: 100,
            scale: 1.0,
            active_workspace: ws(workspace),
            focused,
            ..Monitor::default()
        }
    }

    fn client(address: &str, class: &str, monitor: i32, workspace: i32, size: [i32; 2]) -> Client {
        Client {
            address: address.into(),
            class: class.into(),
            title: format!("{class} title"),
            monitor,
            workspace: ws(workspace),
            size,
            pid: 4000,
            mapped: true,
            ..Client::default()
        }
    }

    fn two_monitors() -> Snapshot {
        let kitty = client("0x1", "kitty", 0, 1, [50, 50]);
        Snapshot {
            clients: vec![
                kitty.clone(),
                client("0x2", "firefox", 1, 2, [10, 10]),
                client("0x3", "slack", 1, 2, [80, 60]),
                client("0x4", "gimp", 1, 5, [100, 100]),
            ],
            monitors: vec![
                monitor(0, "DP-1", 1, true),
                monitor(1, "HDMI-A-1", 2, false),
            ],
            active: Some(kitty),
        }
    }

    #[test]
    fn terminal_classes_match_case_insensitively() {
        for class in [
            "kitty",
            "Alacritty",
            "foot",
            "org.wezfurlong.wezterm",
            "com.mitchellh.ghostty",
            "konsole",
        ] {
            assert!(is_terminal(class), "{class}");
        }
        for class in ["firefox", "kitty-ish", "", "xterm-kitty"] {
            assert!(!is_terminal(class), "{class}");
        }
    }

    #[test]
    fn context_uses_active_window_on_its_monitor() {
        let snap = two_monitors();
        let ctx = window_context(&snap, &snap.monitors[0]);
        assert_eq!(ctx.class, "kitty");
        assert_eq!(ctx.title, "kitty title");
        assert_eq!(ctx.workspace, "1");
        assert_eq!(ctx.output, "DP-1");
        assert_eq!(ctx.pid, Some(4000));
        assert_eq!(ctx.cwd, None);
    }

    #[test]
    fn context_on_other_monitor_uses_largest_visible_window() {
        let snap = two_monitors();
        let ctx = window_context(&snap, &snap.monitors[1]);
        assert_eq!(ctx.class, "slack");
        assert_eq!(ctx.workspace, "2");
        assert_eq!(ctx.output, "HDMI-A-1");
    }

    #[test]
    fn context_of_empty_monitor_has_workspace_and_output_only() {
        let snap = Snapshot {
            monitors: vec![monitor(0, "DP-1", 3, true)],
            ..Snapshot::default()
        };
        let ctx = window_context(&snap, &snap.monitors[0]);
        assert_eq!(ctx.class, "");
        assert_eq!(ctx.workspace, "3");
        assert_eq!(ctx.output, "DP-1");
        assert_eq!(ctx.pid, None);
    }

    #[test]
    fn focused_context_follows_active_window() {
        let mut snap = two_monitors();
        snap.active = Some(snap.clients[1].clone());
        let ctx = focused_context(&snap);
        assert_eq!(ctx.class, "firefox");
        assert_eq!(ctx.output, "HDMI-A-1");
    }

    #[test]
    fn fullscreen_pause_needs_visible_fullscreen_matching_class() {
        let classes = [Regex::new("^(?:steam_app_.*)$").unwrap()];
        let mut snap = two_monitors();
        snap.clients.push(Client {
            fullscreen: 2,
            ..client("0x9", "steam_app_570", 1, 2, [100, 100])
        });
        assert!(fullscreen_paused(&snap, &classes));

        snap.clients.last_mut().unwrap().fullscreen = 0;
        assert!(!fullscreen_paused(&snap, &classes));

        let hidden_ws = Client {
            fullscreen: 1,
            ..client("0xa", "steam_app_570", 1, 9, [100, 100])
        };
        snap.clients.push(hidden_ws);
        assert!(!fullscreen_paused(&snap, &classes));

        snap.clients[0].fullscreen = 1;
        assert!(!fullscreen_paused(&snap, &classes));
    }
}
