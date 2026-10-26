use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WsRef {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Client {
    pub address: String,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: WsRef,
    pub class: String,
    pub title: String,
    pub pid: i32,
    pub monitor: i32,
    #[serde(deserialize_with = "bool_or_int")]
    pub fullscreen: i32,
    pub mapped: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Monitor {
    pub id: i32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub transform: i32,
    pub active_workspace: WsRef,
    pub special_workspace: WsRef,
    pub focused: bool,
}

impl Client {
    /// Hyprland answers `j/activewindow` with `{}` when nothing is focused.
    pub fn is_empty(&self) -> bool {
        self.address.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BoolOrInt {
    Bool(bool),
    Int(i32),
    Null(()),
}

fn bool_or_int<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
    Ok(match BoolOrInt::deserialize(deserializer)? {
        BoolOrInt::Bool(flag) => i32::from(flag),
        BoolOrInt::Int(value) => value,
        BoolOrInt::Null(()) => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn clients_json_both_fullscreen_shapes() {
        let modern: Vec<Client> = serde_json::from_str(&fixture("clients.json")).unwrap();
        assert_eq!(
            modern.iter().map(|c| c.fullscreen).collect::<Vec<_>>(),
            [0, 2, 0]
        );

        let legacy: Vec<Client> =
            serde_json::from_str(&fixture("clients_legacy_bool.json")).unwrap();
        assert_eq!(
            legacy.iter().map(|c| c.fullscreen).collect::<Vec<_>>(),
            [1, 0]
        );
    }

    #[test]
    fn clients_fields_are_read() {
        let clients: Vec<Client> = serde_json::from_str(&fixture("clients.json")).unwrap();
        let kitty = &clients[0];
        assert_eq!(kitty.address, "0x5581d3a0c2f0");
        assert_eq!(kitty.at, [12, 44]);
        assert_eq!(kitty.size, [1896, 1024]);
        assert_eq!(
            kitty.workspace,
            WsRef {
                id: 1,
                name: "1".into()
            }
        );
        assert_eq!(kitty.class, "kitty");
        assert_eq!(kitty.title, "nvim ~/projects/capricorn, main");
        assert_eq!(kitty.pid, 4821);
        assert!(kitty.mapped && !kitty.hidden);
        assert!(clients[2].hidden && !clients[2].mapped);
        assert_eq!(clients[2].workspace.name, "special:scratch");
    }

    #[test]
    fn clients_ignore_unknown_and_tolerate_missing_fields() {
        let legacy: Vec<Client> =
            serde_json::from_str(&fixture("clients_legacy_bool.json")).unwrap();
        assert_eq!(legacy[1].class, "foot");
        assert!(!legacy[1].hidden);
    }

    #[test]
    fn monitors_use_camel_case_workspaces() {
        let monitors: Vec<Monitor> = serde_json::from_str(&fixture("monitors.json")).unwrap();
        assert_eq!(monitors.len(), 2);
        let first = &monitors[0];
        assert_eq!((first.id, first.name.as_str()), (0, "DP-1"));
        assert_eq!((first.width, first.height), (3840, 2160));
        assert_eq!(first.scale, 1.5);
        assert!(first.focused);
        assert_eq!(first.active_workspace.id, 1);
        let second = &monitors[1];
        assert_eq!((second.x, second.transform), (2560, 1));
        assert_eq!(second.special_workspace.name, "special:scratch");
        assert!(!second.focused);
    }

    #[test]
    fn activewindow_parses_and_empty_object_is_empty_client() {
        let active: Client = serde_json::from_str(&fixture("activewindow.json")).unwrap();
        assert!(!active.is_empty());
        assert_eq!(active.class, "kitty");

        let none: Client = serde_json::from_str(&fixture("activewindow_none.json")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn null_fullscreen_is_zero() {
        let client: Client = serde_json::from_str(r#"{"fullscreen": null}"#).unwrap();
        assert_eq!(client.fullscreen, 0);
    }
}
