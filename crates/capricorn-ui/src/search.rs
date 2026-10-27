use std::collections::HashMap;

use capricorn_core::Secret;
use capricorn_core::proto::{Hit, Kind, RecordRef, SearchQuery};
use iced::widget::{Column, Space, button, column, container, image, row, space, text, text_input};
use iced::{Alignment, Element, Length, Padding};

use crate::format::{Clock, lock_countdown, relative_time};
use crate::icons::{Icon, icon};
use crate::style;
use crate::unlock::{self, Gate, UnlockState};

pub const QUERY_INPUT: &str = "search-query";
pub const WIDTH: u32 = 720;
pub const MAX_HEIGHT: u32 = 520;

const HEADER_HEIGHT: u32 = 56;
const CHIPS_HEIGHT: u32 = 46;
const ROW_HEIGHT: u32 = 60;
const FOOTER_HEIGHT: u32 = 36;
const EMPTY_HEIGHT: u32 = 64;
const LOCKED_BODY_HEIGHT: u32 = 44;
const VISIBLE_ROWS: usize =
    ((MAX_HEIGHT - HEADER_HEIGHT - CHIPS_HEIGHT - FOOTER_HEIGHT - 4) / ROW_HEIGHT) as usize;
const RESULT_LIMIT: u32 = 30;
const APP_CHIPS: usize = 3;
const WEEK_MS: i64 = 7 * 24 * 3600 * 1000;
const THUMB_CACHE_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Screens,
    Clipboard,
    App(String),
}

#[derive(Debug, Default)]
pub struct SearchState {
    pub unlock: UnlockState,
    gate: Option<Gate>,
    query: String,
    filter: Filter,
    this_week: bool,
    hits: Vec<Hit>,
    app_chips: Vec<String>,
    selected: usize,
    thumbs: HashMap<RecordRef, image::Handle>,
    locks_in_secs: Option<u64>,
    notice: Option<String>,
    generation: u64,
}

#[derive(Clone, Debug)]
pub enum Message {
    Unlock(unlock::Message),
    QueryChanged(String),
    Debounced(u64),
    Filter(Filter),
    ToggleWeek,
    Pick(usize),
    Move(i32),
    Open,
    Copy,
    Reopen,
    Close,
}

#[derive(Debug)]
pub enum Effect {
    Unlock(Secret),
    Debounce(u64),
    Search(SearchQuery),
    Open(Hit),
    Copy(String),
    Reopen(Hit),
    Close,
}

impl SearchState {
    /// `None` once this UI's connection is unlocked.
    pub fn set_gate(&mut self, gate: Option<Gate>) {
        if gate.is_some() {
            self.hits.clear();
            self.thumbs.clear();
            self.app_chips.clear();
            if matches!(self.filter, Filter::App(_)) {
                self.filter = Filter::All;
            }
            self.selected = 0;
            self.locks_in_secs = None;
        }
        self.gate = gate;
    }

    pub fn is_locked(&self) -> bool {
        self.gate.is_some()
    }

    pub fn set_locks_in(&mut self, secs: Option<u64>) {
        self.locks_in_secs = secs;
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Stores results and returns the screen hits that still need a thumbnail.
    pub fn set_hits(&mut self, hits: Vec<Hit>) -> Vec<RecordRef> {
        if !matches!(self.filter, Filter::App(_)) {
            self.app_chips = top_apps(&hits, APP_CHIPS);
        }
        self.hits = hits;
        self.selected = 0;
        if self.thumbs.len() > THUMB_CACHE_LIMIT {
            self.thumbs.clear();
        }
        self.hits
            .iter()
            .filter(|h| h.kind == Kind::Screen && !self.thumbs.contains_key(&h.r))
            .map(|h| h.r)
            .collect()
    }

    pub fn set_thumb(&mut self, r: RecordRef, handle: image::Handle) {
        self.thumbs.insert(r, handle);
    }

    pub fn selected_hit(&self) -> Option<&Hit> {
        self.hits.get(self.selected)
    }

    pub fn reset(&mut self) {
        self.query.clear();
        self.filter = Filter::All;
        self.this_week = false;
        self.hits.clear();
        self.app_chips.clear();
        self.selected = 0;
        self.notice = None;
    }

    pub fn update(&mut self, message: Message, clock: &Clock) -> Option<Effect> {
        self.notice = None;
        match message {
            Message::Unlock(message) => self.unlock.update(message).map(Effect::Unlock),
            Message::QueryChanged(query) => {
                self.query = query;
                self.generation += 1;
                Some(Effect::Debounce(self.generation))
            }
            Message::Debounced(generation) if generation == self.generation => self.search(clock),
            Message::Debounced(_) => None,
            Message::Filter(filter) => {
                self.filter = filter;
                self.search(clock)
            }
            Message::ToggleWeek => {
                self.this_week = !self.this_week;
                self.search(clock)
            }
            Message::Pick(index) => {
                self.selected = index.min(self.hits.len().saturating_sub(1));
                self.selected_hit().cloned().map(Effect::Open)
            }
            Message::Move(delta) => {
                self.move_selection(delta);
                None
            }
            Message::Open => self.selected_hit().cloned().map(Effect::Open),
            Message::Copy => self
                .selected_hit()
                .map(|hit| Effect::Copy(hit.snippet.clone())),
            Message::Reopen => self.selected_hit().cloned().map(Effect::Reopen),
            Message::Close => Some(Effect::Close),
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let last = self.hits.len().saturating_sub(1);
        self.selected = self
            .selected
            .saturating_add_signed(delta as isize)
            .min(last);
    }

    fn search(&mut self, clock: &Clock) -> Option<Effect> {
        if self.query.trim().is_empty() || self.is_locked() {
            self.hits.clear();
            return None;
        }
        Some(Effect::Search(self.query_for(clock)))
    }

    fn query_for(&self, clock: &Clock) -> SearchQuery {
        let (kind, app) = match &self.filter {
            Filter::All => (None, None),
            Filter::Screens => (Some(Kind::Screen), None),
            Filter::Clipboard => (Some(Kind::Clipboard), None),
            Filter::App(app) => (None, Some(app.clone())),
        };
        SearchQuery {
            text: self.query.trim().to_string(),
            kind,
            app,
            workspace: None,
            from: self.this_week.then_some(clock.now - WEEK_MS),
            to: None,
            limit: RESULT_LIMIT,
        }
    }

    /// Surface height that fits the current content.
    pub fn height(&self) -> u32 {
        if self.is_locked() {
            return HEADER_HEIGHT + 1 + LOCKED_BODY_HEIGHT + FOOTER_HEIGHT + 2;
        }
        let body = match self.hits.len() {
            0 => EMPTY_HEIGHT,
            n => n.min(VISIBLE_ROWS) as u32 * ROW_HEIGHT,
        };
        HEADER_HEIGHT + CHIPS_HEIGHT + body + FOOTER_HEIGHT + 4
    }

    pub fn view<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        let content = match &self.gate {
            Some(gate) => self.locked_view(gate),
            None => self.unlocked_view(clock),
        };
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style::card)
            .into()
    }

    fn locked_view<'a>(&'a self, gate: &'a Gate) -> Element<'a, Message> {
        let header = container(self.unlock.header(gate).map(Message::Unlock))
            .padding([0, 18])
            .height(HEADER_HEIGHT)
            .align_y(Alignment::Center);
        let body = container(self.unlock.body(gate).map(Message::Unlock))
            .padding([12, 18])
            .height(LOCKED_BODY_HEIGHT);
        let hint = if *gate == Gate::Locked {
            "unlock  ·  Esc close"
        } else {
            "Esc close"
        };
        column![
            header,
            divider(),
            body,
            space::vertical(),
            footer(hint, None)
        ]
        .into()
    }

    fn unlocked_view<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        column![
            self.header(),
            divider(),
            self.chips(),
            divider(),
            self.results(clock),
            space::vertical(),
            footer(
                "open in timeline  ·  Ctrl+C copy text  ·  Ctrl+O reopen app here  ·  Esc close",
                self.notice.as_deref(),
            ),
        ]
        .into()
    }

    fn header(&self) -> Element<'_, Message> {
        let input = text_input("Search your screen and clipboard history", &self.query)
            .id(QUERY_INPUT)
            .on_input(Message::QueryChanged)
            .on_submit(Message::Open)
            .size(17)
            .padding([6, 0])
            .style(style::bare_input);
        container(
            row![
                icon(Icon::Search, 18.0, style::TEXT_MUTED),
                input,
                self.lock_status(),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([0, 18])
        .height(HEADER_HEIGHT)
        .align_y(Alignment::Center)
        .into()
    }

    fn lock_status(&self) -> Element<'_, Message> {
        let label = match self.locks_in_secs {
            Some(secs) => format!("unlocked · {}", lock_countdown(secs)),
            None => "unlocked".to_string(),
        };
        row![
            icon(Icon::LockOpen, 14.0, style::SUCCESS),
            text(label).size(13).color(style::SUCCESS),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    }

    fn chips(&self) -> Element<'_, Message> {
        let fixed = [
            ("All", Filter::All),
            ("Screens", Filter::Screens),
            ("Clipboard", Filter::Clipboard),
        ];
        let mut chips: Vec<Element<'_, Message>> = fixed
            .into_iter()
            .map(|(label, filter)| self.chip(label.to_string(), filter))
            .collect();
        chips.extend(
            self.app_chips
                .iter()
                .map(|app| self.chip(app.clone(), Filter::App(app.clone()))),
        );
        chips.push(chip_button(
            "this week".to_string(),
            self.this_week,
            Message::ToggleWeek,
        ));
        container(row(chips).spacing(8).align_y(Alignment::Center))
            .padding([0, 18])
            .height(CHIPS_HEIGHT)
            .align_y(Alignment::Center)
            .into()
    }

    fn chip(&self, label: String, filter: Filter) -> Element<'_, Message> {
        let active = self.filter == filter;
        chip_button(label, active, Message::Filter(filter))
    }

    fn results<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        if self.hits.is_empty() {
            let line = if self.query.trim().is_empty() {
                "Type to search what you have seen and copied."
            } else {
                "No matches."
            };
            return container(text(line).size(14).color(style::TEXT_MUTED))
                .padding([0, 18])
                .height(EMPTY_HEIGHT)
                .align_y(Alignment::Center)
                .into();
        }
        let first = (self.selected + 1).saturating_sub(VISIBLE_ROWS);
        let rows = self
            .hits
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE_ROWS)
            .map(|(index, hit)| self.result_row(index, hit, clock));
        Column::with_children(rows).into()
    }

    fn result_row<'a>(&'a self, index: usize, hit: &'a Hit, clock: &Clock) -> Element<'a, Message> {
        let snippet = row(snippet_parts(&hit.snippet, &hit.highlights)
            .into_iter()
            .map(|(part, marked)| snippet_part(part, marked)))
        .align_y(Alignment::Center);
        let meta = text(meta_line(hit, clock))
            .size(12)
            .color(style::TEXT_MUTED)
            .wrapping(text::Wrapping::None);
        let body = column![snippet, meta]
            .spacing(4)
            .width(Length::Fill)
            .clip(true);
        let content = row![self.thumbnail(hit), body]
            .spacing(14)
            .align_y(Alignment::Center);
        let content = container(content)
            .height(Length::Fill)
            .align_y(Alignment::Center);
        button(content)
            .on_press(Message::Pick(index))
            .padding(Padding::from([0, 18]))
            .width(Length::Fill)
            .height(ROW_HEIGHT)
            .style(style::row(index == self.selected))
            .into()
    }

    fn thumbnail<'a>(&'a self, hit: &'a Hit) -> Element<'a, Message> {
        if let Some(handle) = self.thumbs.get(&hit.r) {
            return image(handle.clone())
                .width(64)
                .height(40)
                .content_fit(iced::ContentFit::Cover)
                .border_radius(6)
                .into();
        }
        let label: Element<'a, Message> = match hit.kind {
            Kind::Clipboard => icon(Icon::Clipboard, 16.0, style::TEXT_MUTED).into(),
            Kind::Screen => column![
                icon(Icon::Monitor, 13.0, style::TEXT_FAINT),
                text(short_app(&hit.app)).size(10).color(style::TEXT_MUTED),
            ]
            .spacing(1)
            .align_x(Alignment::Center)
            .into(),
        };
        container(label)
            .center_x(64)
            .center_y(40)
            .style(style::fill(style::SURFACE_RAISED, 6.0))
            .into()
    }
}

fn snippet_part<'a>(part: &str, marked: bool) -> Element<'a, Message> {
    let part = text(part.replace('\n', " "))
        .size(15)
        .wrapping(text::Wrapping::None);
    if marked {
        container(part.color(style::MARK_TEXT))
            .padding([0, 2])
            .style(style::fill(style::MARK_BG, 3.0))
            .into()
    } else {
        part.color(style::TEXT).into()
    }
}

fn chip_button<'a>(label: String, active: bool, message: Message) -> Element<'a, Message> {
    button(text(label).size(13))
        .on_press(message)
        .padding([4, 12])
        .style(style::chip(active))
        .into()
}

fn divider<'a, M: 'a>() -> Element<'a, M> {
    container(Space::new().height(1))
        .width(Length::Fill)
        .height(1)
        .style(style::divider)
        .into()
}

fn footer<'a, M: 'a>(hint: &'a str, notice: Option<&'a str>) -> Element<'a, M> {
    let notice: Element<'a, M> = match notice {
        Some(notice) => text(notice).size(12).color(style::SUCCESS).into(),
        None => Space::new().into(),
    };
    column![
        divider(),
        container(
            row![
                icon(Icon::Enter, 13.0, style::TEXT_FAINT),
                text(hint).size(12).color(style::TEXT_FAINT),
                space::horizontal(),
                notice,
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([0, 18])
        .height(FOOTER_HEIGHT - 1)
        .align_y(Alignment::Center),
    ]
    .into()
}

fn short_app(app: &str) -> String {
    let name = app.rsplit('.').next().unwrap_or(app).to_lowercase();
    name.chars().take(9).collect()
}

pub fn meta_line(hit: &Hit, clock: &Clock) -> String {
    let when = relative_time(hit.ts, clock);
    match hit.kind {
        Kind::Clipboard => format!("Clipboard · copied from {} · {when}", hit.app),
        Kind::Screen => {
            let mut parts = vec![hit.app.clone()];
            if !hit.workspace.is_empty() {
                parts.push(format!("ws {}", hit.workspace));
            }
            parts.extend(hit.context.clone().filter(|c| !c.is_empty()));
            parts.push(when);
            parts.join(" · ")
        }
    }
}

/// Splits `snippet` into plain and highlighted parts. Ranges that overlap, run past the end
/// or cut a character in half are dropped.
pub fn snippet_parts<'a>(snippet: &'a str, highlights: &[(u32, u32)]) -> Vec<(&'a str, bool)> {
    let mut ranges: Vec<(usize, usize)> = highlights
        .iter()
        .map(|&(start, end)| (start as usize, end as usize))
        .filter(|&(start, end)| {
            start < end
                && end <= snippet.len()
                && snippet.is_char_boundary(start)
                && snippet.is_char_boundary(end)
        })
        .collect();
    ranges.sort_unstable();
    let mut parts = Vec::new();
    let mut cursor = 0;
    for (start, end) in ranges {
        if start < cursor {
            continue;
        }
        if start > cursor {
            parts.push((&snippet[cursor..start], false));
        }
        parts.push((&snippet[start..end], true));
        cursor = end;
    }
    if cursor < snippet.len() {
        parts.push((&snippet[cursor..], false));
    }
    parts
}

pub fn top_apps(hits: &[Hit], limit: usize) -> Vec<String> {
    let screens = hits.iter().filter(|h| h.kind == Kind::Screen);
    let mut apps = rank_apps(screens.map(|h| h.app.as_str()));
    apps.truncate(limit);
    apps
}

/// Most frequent apps first; ties keep the order of first appearance.
pub fn rank_apps<'a>(apps: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for app in apps {
        match counts.iter_mut().find(|(seen, _)| *seen == app) {
            Some((_, count)) => *count += 1,
            None => counts.push((app, 1)),
        }
    }
    counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    counts.into_iter().map(|(app, _)| app.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn snippet_parts_split_on_highlights() {
        let parts = snippet_parts("$ nix flake lock", &[(12, 16), (2, 11), (6, 14)]);
        assert_eq!(
            parts,
            [
                ("$ ", false),
                ("nix flake", true),
                (" ", false),
                ("lock", true)
            ]
        );
    }

    #[test]
    fn snippet_parts_drop_bad_ranges() {
        let snippet = "caf\u{e9} ok";
        let parts = snippet_parts(snippet, &[(3, 4), (6, 99), (2, 2)]);
        assert_eq!(parts, [(snippet, false)]);
    }

    #[test]
    fn top_apps_by_count_then_first_seen() {
        let apps = top_apps(&fixtures::hits(), 3);
        assert_eq!(apps, ["kitty", "firefox", "vesktop"]);
    }

    #[test]
    fn typing_debounces_and_only_latest_searches() {
        let clock = fixtures::clock();
        let mut state = SearchState::default();
        state.set_gate(None);
        let first = state.update(Message::QueryChanged("nix".into()), &clock);
        assert!(matches!(first, Some(Effect::Debounce(1))));
        state.update(Message::QueryChanged("nix flake".into()), &clock);
        assert!(state.update(Message::Debounced(1), &clock).is_none());
        let Some(Effect::Search(query)) = state.update(Message::Debounced(2), &clock) else {
            panic!("expected a search");
        };
        assert_eq!(query.text, "nix flake");
        assert_eq!(query.kind, None);
    }

    #[test]
    fn filters_shape_the_query() {
        let clock = fixtures::clock();
        let mut state = SearchState::default();
        state.set_gate(None);
        state.update(Message::QueryChanged("lock".into()), &clock);
        let Some(Effect::Search(query)) = state.update(Message::Filter(Filter::Clipboard), &clock)
        else {
            panic!("expected a search");
        };
        assert_eq!(query.kind, Some(Kind::Clipboard));
        let Some(Effect::Search(query)) = state.update(Message::ToggleWeek, &clock) else {
            panic!("expected a search");
        };
        assert_eq!(query.from, Some(clock.now - WEEK_MS));
    }

    #[test]
    fn locked_overlay_never_searches() {
        let clock = fixtures::clock();
        let mut state = SearchState::default();
        state.set_gate(Some(Gate::Locked));
        state.update(Message::QueryChanged("nix".into()), &clock);
        assert!(state.update(Message::Debounced(1), &clock).is_none());
    }

    #[test]
    fn selection_moves_and_opens_hit() {
        let clock = fixtures::clock();
        let mut state = SearchState::default();
        state.set_gate(None);
        let missing = state.set_hits(fixtures::hits());
        assert_eq!(missing.len(), 3, "clipboard hits have no thumbnail");
        state.update(Message::Move(1), &clock);
        state.update(Message::Move(5), &clock);
        state.update(Message::Move(-1), &clock);
        let Some(Effect::Open(hit)) = state.update(Message::Open, &clock) else {
            panic!("expected open");
        };
        assert_eq!(hit.app, "firefox");
        let Some(Effect::Copy(text)) = state.update(Message::Copy, &clock) else {
            panic!("expected copy");
        };
        assert!(text.contains("refresh the lock"));
    }

    #[test]
    fn height_tracks_results() {
        let mut state = SearchState::default();
        state.set_gate(None);
        let empty = state.height();
        state.set_hits(fixtures::hits());
        assert_eq!(state.height(), empty - EMPTY_HEIGHT + 4 * ROW_HEIGHT);
        let many = std::iter::repeat_n(fixtures::hits(), 10)
            .flatten()
            .collect();
        state.set_hits(many);
        assert!(state.height() <= MAX_HEIGHT);
    }

    #[test]
    fn meta_line_formats() {
        let clock = fixtures::clock();
        let hits = fixtures::hits();
        assert_eq!(
            meta_line(&hits[0], &clock),
            "kitty · ws 2 · ~/projects/dotfiles · today 14:32"
        );
        assert_eq!(
            meta_line(&hits[1], &clock),
            "Clipboard · copied from firefox · Mon 09:10"
        );
    }
}
