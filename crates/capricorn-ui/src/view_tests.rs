use std::path::{Path, PathBuf};

use iced::widget::{container, image};
use iced::{Element, Length, Settings, Size};
use iced_test::{Simulator, selector};

use crate::fixtures;
use crate::format::Clock;
use crate::search::{self, SearchState};
use crate::style;
use crate::timeline::{self, TimelineState};
use crate::unlock::Gate;

const OVERLAY_MARGIN: f32 = 40.0;
const TIMELINE_SIZE: Size = Size::new(1232.0, 760.0);

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-snapshots")
}

/// Rewrites `<name>-<renderer>.png` so the file always shows the current UI.
fn save_snapshot<M>(ui: &mut Simulator<'_, M>, name: &str) {
    let dir = snapshot_dir();
    std::fs::create_dir_all(&dir).unwrap();
    for stale in std::fs::read_dir(&dir).unwrap().flatten() {
        if stale
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{name}-"))
        {
            std::fs::remove_file(stale.path()).unwrap();
        }
    }
    let snapshot = ui.snapshot(&style::theme()).unwrap();
    assert!(
        snapshot
            .matches_image(dir.join(format!("{name}.png")))
            .unwrap()
    );
}

fn overlay<'a>(state: &'a SearchState, clock: &'a Clock) -> Simulator<'a, search::Message> {
    let height = state.height() as f32;
    let element: Element<'a, search::Message> = container(
        container(state.view(clock))
            .width(search::WIDTH as f32)
            .height(height),
    )
    .center(Length::Fill)
    .into();
    let size = Size::new(
        search::WIDTH as f32 + 2.0 * OVERLAY_MARGIN,
        height + 2.0 * OVERLAY_MARGIN,
    );
    Simulator::with_size(Settings::default(), size, element)
}

fn unlocked_search_with_hits() -> SearchState {
    let mut state = SearchState::default();
    state.set_gate(None);
    state.set_locks_in(Some(14 * 60));
    let clock = fixtures::clock();
    state.update(
        search::Message::QueryChanged("nix flake update lock".into()),
        &clock,
    );
    let missing = state.set_hits(fixtures::hits());
    let thumb = image::Handle::from_rgba(320, 200, fixtures::fake_screenshot(320, 200));
    state.set_thumb(missing[0], thumb);
    state
}

#[test]
fn search_view_locked_shows_password() {
    let clock = fixtures::clock();
    let mut state = SearchState::default();
    state.set_gate(Some(Gate::Locked));
    let mut ui = overlay(&state, &clock);
    assert!(ui.find(selector::id(crate::unlock::PASSWORD_INPUT)).is_ok());
    assert!(ui.find("locked").is_ok());
    assert!(ui.find(selector::id(search::QUERY_INPUT)).is_err());
    save_snapshot(&mut ui, "search-locked");
}

#[test]
fn search_view_renders_hits() {
    let clock = fixtures::clock();
    let state = unlocked_search_with_hits();
    let mut ui = overlay(&state, &clock);
    assert!(
        ui.find("kitty · ws 2 · ~/projects/dotfiles · today 14:32")
            .is_ok()
    );
    assert!(
        ui.find("Clipboard · copied from firefox · Mon 09:10")
            .is_ok()
    );
    assert!(ui.find("nix flake").is_ok());
    assert!(ui.find(" --commit-").is_ok());
    assert!(ui.find("unlocked · locks in 14m").is_ok());
    save_snapshot(&mut ui, "search-hits");
}

#[test]
fn clicking_a_hit_opens_it() {
    let clock = fixtures::clock();
    let state = unlocked_search_with_hits();
    let mut ui = overlay(&state, &clock);
    ui.click("vesktop · ws 4 · #nixos · Sun 21:47").unwrap();
    let picked: Vec<_> = ui.into_messages().collect();
    assert!(
        matches!(picked.as_slice(), [search::Message::Pick(3)]),
        "{picked:?}"
    );
}

fn timeline_day() -> TimelineState {
    let clock = fixtures::clock();
    let never = vec![
        "KeePassXC".to_string(),
        "Private browser windows".to_string(),
    ];
    let mut state = TimelineState::new(&clock, never);
    state.set_gate(None);
    state.set_locks_in(Some(14 * 60));
    state.show_day(clock.today(), None, &clock);
    state.set_entries(fixtures::day_entries());
    visit_workspaces(&mut state, &clock);
    state.update(timeline::Message::ScrubTo(fixtures::hits()[0].ts), &clock);
    let selected = state.selected().unwrap().clone();
    state.set_detail(fixtures::detail(&selected, "2"));
    let shot = image::Handle::from_rgba(1280, 720, fixtures::fake_screenshot(1280, 720));
    state.set_image(selected.r, shot);
    state
}

/// The sidebar learns workspaces from the moments the user looked at.
fn visit_workspaces(state: &mut TimelineState, clock: &Clock) {
    for (hour, workspace) in [(9, "1"), (10, "3"), (12, "5"), (15, "4"), (14, "2")] {
        state.update(
            timeline::Message::ScrubTo(fixtures::at(27, hour, 30, 0)),
            clock,
        );
        let selected = state.selected().unwrap().clone();
        state.set_detail(fixtures::detail(&selected, workspace));
    }
}

#[test]
fn timeline_renders_day() {
    let clock = fixtures::clock();
    let state = timeline_day();
    let mut ui = Simulator::with_size(Settings::default(), TIMELINE_SIZE, state.view(&clock));
    assert!(
        ui.find("kitty · ws 2 · ~/projects/dotfiles · 14:32:08")
            .is_ok()
    );
    assert!(ui.find("1 · 2 · 3 · 4 · 5").is_ok());
    assert!(ui.find("Tue 27 Oct").is_ok());
    save_snapshot(&mut ui, "timeline-day");
}

#[test]
fn timeline_locked_shows_password() {
    let clock = fixtures::clock();
    let mut state = TimelineState::new(&clock, Vec::new());
    state.set_gate(Some(Gate::Locked));
    let mut ui = Simulator::with_size(Settings::default(), TIMELINE_SIZE, state.view(&clock));
    assert!(ui.find(selector::id(crate::unlock::PASSWORD_INPUT)).is_ok());
}
