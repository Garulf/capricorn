use std::time::{Duration, Instant};

use capricorn_core::rules::RuleSet;
use capricorn_hypr::{Client, Monitor};
use image::RgbaImage;

/// How long frames stay unsafe after an excluded window left the place it was shown in.
/// Covers fade-out and slide animations Hyprland no longer reports.
pub const EXCLUDED_GRACE: Duration = Duration::from_secs(2);

/// Hyprland state read just before or just after grabbing the outputs.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub clients: Vec<Client>,
    pub monitors: Vec<Monitor>,
    pub active: Option<Client>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    None,
    Partial,
    Full,
}

impl Snapshot {
    /// Mapped, unhidden clients on a workspace some monitor is currently showing.
    pub fn visible_clients(&self) -> Vec<Client> {
        self.clients
            .iter()
            .filter(|c| is_drawn(c) && self.workspace_shown(c.workspace.id))
            .cloned()
            .collect()
    }

    fn workspace_shown(&self, id: i32) -> bool {
        self.monitors.iter().any(|m| {
            m.active_workspace.id == id
                || (m.special_workspace.id != 0 && m.special_workspace.id == id)
        })
    }
}

pub fn is_excluded(client: &Client, rules: &RuleSet) -> bool {
    rules.matches(&client.class, &client.title, &client.workspace.name)
}

/// The focused window's popups, menus and tooltips can reach past its rectangle, so an
/// excluded window with focus makes every frame unsafe.
pub fn focus_is_excluded(snap: &Snapshot, rules: &RuleSet) -> bool {
    focused_excluded(snap, rules).is_some()
}

/// Remembers where excluded windows were last shown. Once one closes, moves, or its
/// workspace is switched away, Hyprland reports the end state while the old one is still
/// being animated out, so frames are dropped for `EXCLUDED_GRACE` after that is noticed.
#[derive(Debug, Default)]
pub struct ExclusionMemory {
    shown: Vec<ShownPlacement>,
    left_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShownPlacement {
    address: String,
    at: [i32; 2],
    size: [i32; 2],
    workspace: i32,
}

impl ExclusionMemory {
    pub fn observe(&mut self, snap: &Snapshot, rules: &RuleSet, now: Instant) {
        let shown: Vec<_> = snap
            .visible_clients()
            .into_iter()
            .filter(|c| is_excluded(c, rules))
            .map(|c| ShownPlacement {
                address: c.address,
                at: c.at,
                size: c.size,
                workspace: c.workspace.id,
            })
            .collect();
        if self.shown.iter().any(|old| !shown.contains(old)) {
            self.left_at = Some(now);
        }
        self.shown = shown;
    }

    /// True while an excluded window may still be drawn where it no longer is.
    pub fn unsettled(&self, now: Instant) -> bool {
        self.left_at
            .is_some_and(|at| now.saturating_duration_since(at) < EXCLUDED_GRACE)
    }
}

fn is_drawn(client: &Client) -> bool {
    client.mapped && !client.hidden
}

/// Blacks out every excluded client's region on `mon`. `img` is the grab of that monitor.
/// When the grab's orientation does not match the monitor's layout, no region can be
/// trusted and any overlapping excluded window makes the whole frame unusable.
pub fn blackout(
    img: &mut RgbaImage,
    mon: &Monitor,
    clients: &[Client],
    rules: &RuleSet,
) -> Coverage {
    let view = MonitorView::new(mon, img.width(), img.height());
    let mut coverage = Coverage::None;
    for client in clients
        .iter()
        .filter(|c| is_drawn(c) && is_excluded(c, rules))
    {
        let Some(region) = view.area.intersect(&LogicalRect::of_client(client)) else {
            continue;
        };
        let Some(scale) = view.pixels_per_unit else {
            return Coverage::Full;
        };
        if region == view.area {
            return Coverage::Full;
        }
        fill_black(
            img,
            region.to_pixels(&view.area, scale, img.width(), img.height()),
        );
        coverage = Coverage::Partial;
    }
    coverage
}

/// True when nothing that decides what gets blacked out changed between the two reads.
pub fn stable(before: &Snapshot, after: &Snapshot, rules: &RuleSet) -> bool {
    before.monitors == after.monitors
        && excluded_placement(before, rules) == excluded_placement(after, rules)
        && focused_excluded(before, rules) == focused_excluded(after, rules)
}

type Placement<'a> = (&'a str, [i32; 2], [i32; 2], i32, bool);

fn excluded_placement<'a>(snap: &'a Snapshot, rules: &RuleSet) -> Vec<Placement<'a>> {
    let mut placements: Vec<_> = snap
        .clients
        .iter()
        .filter(|c| is_excluded(c, rules))
        .map(|c| {
            (
                c.address.as_str(),
                c.at,
                c.size,
                c.workspace.id,
                is_drawn(c),
            )
        })
        .collect();
    placements.sort_unstable();
    placements
}

fn focused_excluded<'a>(snap: &'a Snapshot, rules: &RuleSet) -> Option<&'a str> {
    snap.active
        .as_ref()
        .filter(|c| is_excluded(c, rules))
        .map(|c| c.address.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LogicalRect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl LogicalRect {
    fn of_client(client: &Client) -> Self {
        let [x, y] = client.at.map(f64::from);
        let [w, h] = client.size.map(f64::from);
        Self {
            x0: x,
            y0: y,
            x1: x + w,
            y1: y + h,
        }
    }

    fn intersect(&self, other: &Self) -> Option<Self> {
        let rect = Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        };
        (rect.x0 < rect.x1 && rect.y0 < rect.y1).then_some(rect)
    }

    /// Rounds outward so a fractional scale never leaves a sliver of the window visible.
    fn to_pixels(self, origin: &Self, scale: f64, width: u32, height: u32) -> PixelRect {
        let px = |v: f64, max: u32| v.clamp(0.0, f64::from(max)) as u32;
        PixelRect {
            x0: px(((self.x0 - origin.x0) * scale).floor(), width),
            y0: px(((self.y0 - origin.y0) * scale).floor(), height),
            x1: px(((self.x1 - origin.x0) * scale).ceil(), width),
            y1: px(((self.y1 - origin.y0) * scale).ceil(), height),
        }
    }
}

struct PixelRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

/// A monitor in Hyprland's logical layout, plus how logical units map onto the grab.
struct MonitorView {
    area: LogicalRect,
    pixels_per_unit: Option<f64>,
}

const ASPECT_TOLERANCE: f64 = 0.02;

impl MonitorView {
    /// Hyprland reports `width`/`height` in physical pixels before the output transform;
    /// the layout uses those divided by `scale`, swapped for 90 and 270 degree transforms.
    fn new(mon: &Monitor, img_width: u32, img_height: u32) -> Self {
        let scale = if mon.scale > 0.0 { mon.scale } else { 1.0 };
        let (mut w, mut h) = (f64::from(mon.width) / scale, f64::from(mon.height) / scale);
        if mon.transform % 2 == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        let x0 = f64::from(mon.x);
        let y0 = f64::from(mon.y);
        let sx = f64::from(img_width) / w;
        let sy = f64::from(img_height) / h;
        let consistent = w > 0.0 && h > 0.0 && ((sx - sy) / sx).abs() <= ASPECT_TOLERANCE;
        Self {
            area: LogicalRect {
                x0,
                y0,
                x1: x0 + w,
                y1: y0 + h,
            },
            pixels_per_unit: consistent.then_some(sx),
        }
    }
}

fn fill_black(img: &mut RgbaImage, rect: PixelRect) {
    const OPAQUE_BLACK: [u8; 4] = [0, 0, 0, 255];
    let row_len = img.width() as usize * 4;
    let (start, end) = (rect.x0 as usize * 4, rect.x1 as usize * 4);
    for y in rect.y0..rect.y1 {
        let row_start = y as usize * row_len;
        let row = &mut img.as_mut()[row_start + start..row_start + end];
        row.as_chunks_mut::<4>().0.fill(OPAQUE_BLACK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capricorn_hypr::WsRef;
    use image::Rgba;

    const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);

    fn ws(id: i32) -> WsRef {
        WsRef {
            id,
            name: id.to_string(),
        }
    }

    fn client(address: &str, class: &str, at: [i32; 2], size: [i32; 2]) -> Client {
        Client {
            address: address.into(),
            class: class.into(),
            title: format!("{class} window"),
            at,
            size,
            workspace: ws(1),
            mapped: true,
            ..Client::default()
        }
    }

    fn keepass(at: [i32; 2], size: [i32; 2]) -> Client {
        client("0xk", "org.keepassxc.KeePassXC", at, size)
    }

    fn monitor(x: i32, y: i32, width: i32, height: i32, scale: f64) -> Monitor {
        Monitor {
            name: "DP-1".into(),
            x,
            y,
            width,
            height,
            scale,
            active_workspace: ws(1),
            ..Monitor::default()
        }
    }

    fn white(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, WHITE)
    }

    fn snapshot(clients: Vec<Client>) -> Snapshot {
        Snapshot {
            clients,
            monitors: vec![monitor(0, 0, 100, 50, 1.0)],
            active: None,
        }
    }

    #[test]
    fn blackout_partial_window() {
        let mut img = white(100, 50);
        let coverage = blackout(
            &mut img,
            &monitor(0, 0, 100, 50, 1.0),
            &[keepass([10, 10], [20, 10])],
            &RuleSet::defaults(),
        );
        assert_eq!(coverage, Coverage::Partial);
        assert_eq!(*img.get_pixel(10, 10), BLACK);
        assert_eq!(*img.get_pixel(29, 19), BLACK);
        assert_eq!(*img.get_pixel(30, 19), WHITE);
        assert_eq!(*img.get_pixel(29, 20), WHITE);
        assert_eq!(*img.get_pixel(9, 10), WHITE);
    }

    #[test]
    fn blackout_full() {
        let rules = RuleSet::defaults();
        let mon = monitor(0, 0, 100, 50, 1.0);
        let mut exact = white(100, 50);
        assert_eq!(
            blackout(&mut exact, &mon, &[keepass([0, 0], [100, 50])], &rules),
            Coverage::Full
        );
        let mut larger = white(100, 50);
        assert_eq!(
            blackout(&mut larger, &mon, &[keepass([-5, -5], [300, 300])], &rules),
            Coverage::Full
        );
    }

    #[test]
    fn blackout_leaves_normal_windows_alone() {
        let mut img = white(100, 50);
        let coverage = blackout(
            &mut img,
            &monitor(0, 0, 100, 50, 1.0),
            &[client("0x1", "kitty", [0, 0], [100, 50])],
            &RuleSet::defaults(),
        );
        assert_eq!(coverage, Coverage::None);
        assert!(img.pixels().all(|p| *p == WHITE));
    }

    #[test]
    fn blackout_respects_monitor_offset_and_scale() {
        let mut img = white(200, 100);
        let mon = monitor(1000, 50, 200, 100, 2.0);
        let elsewhere = keepass([0, 0], [100, 100]);
        let here = Client {
            address: "0xk2".into(),
            ..keepass([1010, 60], [20, 10])
        };
        let coverage = blackout(&mut img, &mon, &[elsewhere, here], &RuleSet::defaults());
        assert_eq!(coverage, Coverage::Partial);
        assert_eq!(*img.get_pixel(20, 20), BLACK);
        assert_eq!(*img.get_pixel(59, 39), BLACK);
        assert_eq!(*img.get_pixel(60, 39), WHITE);
        assert_eq!(*img.get_pixel(59, 40), WHITE);
        assert_eq!(*img.get_pixel(19, 20), WHITE);
        assert_eq!(*img.get_pixel(0, 0), WHITE);
    }

    #[test]
    fn blackout_fractional_scale_rounds_outward() {
        let mut img = white(150, 75);
        let mon = monitor(0, 0, 150, 75, 1.5);
        blackout(
            &mut img,
            &mon,
            &[keepass([1, 1], [1, 1])],
            &RuleSet::defaults(),
        );
        assert_eq!(*img.get_pixel(1, 1), BLACK);
        assert_eq!(*img.get_pixel(2, 2), BLACK);
        assert_eq!(*img.get_pixel(3, 3), WHITE);
        assert_eq!(*img.get_pixel(0, 0), WHITE);
    }

    #[test]
    fn blackout_with_unexpected_orientation_is_full_when_excluded_window_present() {
        let rotated = Monitor {
            transform: 1,
            ..monitor(0, 0, 100, 50, 1.0)
        };
        let rules = RuleSet::defaults();
        let mut img = white(100, 50);
        assert_eq!(
            blackout(&mut img, &rotated, &[keepass([0, 0], [5, 5])], &rules),
            Coverage::Full
        );
        let mut img = white(100, 50);
        assert_eq!(
            blackout(
                &mut img,
                &rotated,
                &[client("0x1", "kitty", [0, 0], [5, 5])],
                &rules
            ),
            Coverage::None
        );
    }

    #[test]
    fn blackout_ignores_hidden_and_unmapped_clients() {
        let mut img = white(100, 50);
        let hidden = Client {
            hidden: true,
            ..keepass([0, 0], [10, 10])
        };
        let unmapped = Client {
            mapped: false,
            ..keepass([0, 0], [10, 10])
        };
        let coverage = blackout(
            &mut img,
            &monitor(0, 0, 100, 50, 1.0),
            &[hidden, unmapped],
            &RuleSet::defaults(),
        );
        assert_eq!(coverage, Coverage::None);
    }

    #[test]
    fn visible_clients_follow_shown_workspaces() {
        let mut shown_special = monitor(0, 0, 100, 50, 1.0);
        shown_special.special_workspace = WsRef {
            id: -98,
            name: "special:scratch".into(),
        };
        let on_active = client("0x1", "kitty", [0, 0], [5, 5]);
        let on_other = Client {
            workspace: ws(2),
            ..client("0x2", "foot", [0, 0], [5, 5])
        };
        let on_special = Client {
            workspace: shown_special.special_workspace.clone(),
            ..client("0x3", "pavucontrol", [0, 0], [5, 5])
        };
        let snap = Snapshot {
            clients: vec![on_active, on_other, on_special],
            monitors: vec![shown_special],
            active: None,
        };
        let addresses: Vec<_> = snap
            .visible_clients()
            .into_iter()
            .map(|c| c.address)
            .collect();
        assert_eq!(addresses, ["0x1", "0x3"]);
    }

    #[test]
    fn memory_is_unsettled_after_excluded_window_leaves() {
        let rules = RuleSet::defaults();
        let t = Instant::now();
        let shown = snapshot(vec![keepass([0, 0], [10, 10])]);
        let mut memory = ExclusionMemory::default();
        memory.observe(&shown, &rules, t);
        assert!(!memory.unsettled(t));

        let mut switched = shown.clone();
        switched.monitors[0].active_workspace = ws(2);
        memory.observe(&switched, &rules, t + Duration::from_millis(100));
        assert!(memory.unsettled(t + Duration::from_millis(100)));
        assert!(memory.unsettled(t + Duration::from_millis(2099)));
        assert!(!memory.unsettled(t + Duration::from_millis(2100)));
    }

    #[test]
    fn memory_is_unsettled_after_excluded_window_closes_or_moves() {
        let rules = RuleSet::defaults();
        let t = Instant::now();
        let mut memory = ExclusionMemory::default();
        memory.observe(&snapshot(vec![keepass([0, 0], [10, 10])]), &rules, t);
        memory.observe(&snapshot(vec![]), &rules, t);
        assert!(memory.unsettled(t));

        let later = t + EXCLUDED_GRACE * 2;
        memory.observe(&snapshot(vec![keepass([0, 0], [10, 10])]), &rules, later);
        assert!(!memory.unsettled(later));
        memory.observe(&snapshot(vec![keepass([30, 0], [10, 10])]), &rules, later);
        assert!(memory.unsettled(later));
    }

    #[test]
    fn memory_ignores_other_windows_and_appearing_excluded_ones() {
        let rules = RuleSet::defaults();
        let t = Instant::now();
        let mut memory = ExclusionMemory::default();
        memory.observe(
            &snapshot(vec![client("0x1", "kitty", [0, 0], [5, 5])]),
            &rules,
            t,
        );
        memory.observe(&snapshot(vec![keepass([0, 0], [10, 10])]), &rules, t);
        assert!(!memory.unsettled(t));
    }

    #[test]
    fn focused_excluded_window_is_detected() {
        let rules = RuleSet::defaults();
        let mut snap = snapshot(vec![keepass([0, 0], [10, 10])]);
        assert!(!focus_is_excluded(&snap, &rules));
        snap.active = Some(client("0x1", "kitty", [0, 0], [5, 5]));
        assert!(!focus_is_excluded(&snap, &rules));
        snap.active = Some(keepass([0, 0], [10, 10]));
        assert!(focus_is_excluded(&snap, &rules));
    }

    #[test]
    fn stable_detects_moved_excluded_window() {
        let rules = RuleSet::defaults();
        let before = snapshot(vec![keepass([0, 0], [10, 10])]);
        let after = snapshot(vec![keepass([10, 0], [10, 10])]);
        assert!(!stable(&before, &after, &rules));
        let resized = snapshot(vec![keepass([0, 0], [20, 10])]);
        assert!(!stable(&before, &resized, &rules));
        let moved_workspace = snapshot(vec![Client {
            workspace: ws(2),
            ..keepass([0, 0], [10, 10])
        }]);
        assert!(!stable(&before, &moved_workspace, &rules));
    }

    #[test]
    fn stable_ignores_other_windows() {
        let rules = RuleSet::defaults();
        let before = snapshot(vec![
            keepass([0, 0], [10, 10]),
            client("0x1", "kitty", [20, 0], [10, 10]),
        ]);
        let mut after = before.clone();
        after.clients[1].at = [40, 20];
        after.clients[1].title = "vim notes.md".into();
        after.clients.push(client("0x2", "foot", [0, 30], [5, 5]));
        assert!(stable(&before, &after, &rules));
        assert!(stable(&before, &before.clone(), &rules));
    }

    #[test]
    fn stable_detects_excluded_window_appearing_or_closing() {
        let rules = RuleSet::defaults();
        let without = snapshot(vec![client("0x1", "kitty", [0, 0], [10, 10])]);
        let with = snapshot(vec![
            client("0x1", "kitty", [0, 0], [10, 10]),
            keepass([0, 0], [10, 10]),
        ]);
        assert!(!stable(&without, &with, &rules));
        assert!(!stable(&with, &without, &rules));
    }

    #[test]
    fn stable_detects_window_becoming_private() {
        let rules = RuleSet::defaults();
        let before = snapshot(vec![client("0x1", "firefox", [0, 0], [10, 10])]);
        let mut after = before.clone();
        after.clients[0].title = "Mozilla Firefox Private Browsing".into();
        assert!(!stable(&before, &after, &rules));
    }

    #[test]
    fn stable_detects_workspace_switch() {
        let rules = RuleSet::defaults();
        let before = snapshot(vec![]);
        let mut after = before.clone();
        after.monitors[0].active_workspace = ws(2);
        assert!(!stable(&before, &after, &rules));
    }

    #[test]
    fn stable_detects_focus_onto_or_off_excluded_window() {
        let rules = RuleSet::defaults();
        let kitty = client("0x1", "kitty", [0, 0], [10, 10]);
        let vault = keepass([20, 0], [10, 10]);
        let mut on_kitty = snapshot(vec![kitty.clone(), vault.clone()]);
        on_kitty.active = Some(kitty);
        let mut on_vault = on_kitty.clone();
        on_vault.active = Some(vault);
        assert!(!stable(&on_kitty, &on_vault, &rules));
        assert!(!stable(&on_vault, &on_kitty, &rules));
    }
}
