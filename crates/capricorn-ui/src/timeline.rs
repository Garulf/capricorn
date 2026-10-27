use std::collections::BTreeSet;

use capricorn_core::Secret;
use capricorn_core::proto::{Hit, Kind, RecordDetail, RecordRef, SearchQuery, TimelineEntry, Ts};
use iced::widget::{
    Column, Space, button, canvas, column, container, image, row, scrollable, space, stack, text,
    text_input,
};
use iced::{Alignment, Color, Element, Font, Length};
use jiff::civil::Date;

use crate::format::{Clock, clock_time, day_label, lock_countdown};
use crate::icons::{Icon, icon};
use crate::lanes::{self, Lanes, Segment, axis_range, lane_segments};
use crate::style;
use crate::unlock::{self, Gate, UnlockState};

pub const QUERY_INPUT: &str = "timeline-query";
pub const MARGIN: i32 = 24;
const SIDEBAR_WIDTH: f32 = 180.0;
const MATCH_LIMIT: u32 = 200;

#[derive(Debug)]
pub struct TimelineState {
    pub unlock: UnlockState,
    gate: Option<Gate>,
    day: Date,
    day_bounds: (Ts, Ts),
    entries: Vec<TimelineEntry>,
    segments: Vec<Segment>,
    cursor: Option<usize>,
    target: Option<RecordRef>,
    app_filter: Option<String>,
    detail: Option<RecordDetail>,
    image: Option<(RecordRef, image::Handle)>,
    workspaces: BTreeSet<String>,
    never_recorded: Vec<String>,
    query: String,
    matches: Vec<Ts>,
    match_index: usize,
    confirm_delete: bool,
    locks_in_secs: Option<u64>,
    notice: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Message {
    Unlock(unlock::Message),
    QueryChanged(String),
    QuerySubmit,
    PrevDay,
    NextDay,
    Step(i32),
    ScrubTo(Ts),
    PickApp(String),
    CopyText,
    Reopen,
    Delete,
    ConfirmDelete,
    CancelDelete,
    Close,
}

#[derive(Debug)]
pub enum Effect {
    Unlock(Secret),
    LoadDay { from: Ts, to: Ts },
    Select(RecordRef),
    Search(SearchQuery),
    Copy(String),
    Reopen(RecordDetail),
    Delete(RecordRef),
    Close,
}

impl TimelineState {
    pub fn new(clock: &Clock, never_recorded: Vec<String>) -> Self {
        let day = clock.today();
        Self {
            unlock: UnlockState::default(),
            gate: Some(Gate::Connecting),
            day,
            day_bounds: clock.day_bounds(day),
            entries: Vec::new(),
            segments: Vec::new(),
            cursor: None,
            target: None,
            app_filter: None,
            detail: None,
            image: None,
            workspaces: BTreeSet::new(),
            never_recorded,
            query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            confirm_delete: false,
            locks_in_secs: None,
            notice: None,
        }
    }

    pub fn set_gate(&mut self, gate: Option<Gate>) {
        if gate.is_some() {
            self.entries.clear();
            self.segments.clear();
            self.cursor = None;
            self.detail = None;
            self.image = None;
            self.workspaces.clear();
            self.matches.clear();
            self.app_filter = None;
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

    /// Switches to `day` and asks for its entries; `target` gets the cursor once loaded.
    pub fn show_day(&mut self, day: Date, target: Option<RecordRef>, clock: &Clock) -> Effect {
        self.day = day;
        self.day_bounds = clock.day_bounds(day);
        self.target = target;
        self.matches.clear();
        self.confirm_delete = false;
        let (from, to) = self.day_bounds;
        Effect::LoadDay { from, to }
    }

    pub fn reload(&mut self) -> Effect {
        self.target = self.selected().map(|e| e.r);
        let (from, to) = self.day_bounds;
        Effect::LoadDay { from, to }
    }

    pub fn set_entries(&mut self, mut entries: Vec<TimelineEntry>) -> Option<Effect> {
        entries.sort_by_key(|e| e.ts);
        self.segments = lane_segments(&entries);
        self.entries = entries;
        self.workspaces.clear();
        let target = self.target.take();
        let index = target
            .and_then(|r| self.entries.iter().position(|e| e.r == r))
            .or_else(|| self.entries.len().checked_sub(1));
        self.move_cursor(index)
    }

    pub fn set_detail(&mut self, detail: RecordDetail) {
        if self.selected().is_some_and(|e| e.r == detail.hit.r) {
            if !detail.hit.workspace.is_empty() {
                self.workspaces.insert(detail.hit.workspace.clone());
            }
            self.detail = Some(detail);
        }
    }

    pub fn set_image(&mut self, r: RecordRef, handle: image::Handle) {
        if self.selected().is_some_and(|e| e.r == r) {
            self.image = Some((r, handle));
        }
    }

    pub fn set_matches(&mut self, hits: &[Hit]) -> Option<Effect> {
        let mut matches: Vec<Ts> = hits.iter().map(|h| h.ts).collect();
        matches.sort_unstable();
        self.matches = matches;
        self.match_index = 0;
        if self.matches.is_empty() {
            self.notice = Some("No matches on this day".into());
            return None;
        }
        self.jump_to_match()
    }

    pub fn selected(&self) -> Option<&TimelineEntry> {
        self.cursor.and_then(|i| self.entries.get(i))
    }

    pub fn update(&mut self, message: Message, clock: &Clock) -> Option<Effect> {
        self.notice = None;
        match message {
            Message::Unlock(message) => self.unlock.update(message).map(Effect::Unlock),
            Message::QueryChanged(query) => {
                self.query = query;
                self.matches.clear();
                None
            }
            Message::QuerySubmit => self.submit_query(),
            Message::PrevDay => self
                .day
                .yesterday()
                .ok()
                .map(|d| self.show_day(d, None, clock)),
            Message::NextDay => {
                let next = self.day.tomorrow().ok().filter(|d| *d <= clock.today())?;
                Some(self.show_day(next, None, clock))
            }
            Message::Step(delta) => {
                let index = self.step_index(delta);
                self.move_cursor(index)
            }
            Message::ScrubTo(ts) => {
                let index = nearest_entry(&self.entries, ts, self.app_filter.as_deref());
                self.move_cursor(index)
            }
            Message::PickApp(app) => {
                self.app_filter = (self.app_filter.as_ref() != Some(&app)).then_some(app);
                None
            }
            Message::CopyText => self.detail.as_ref().map(|d| Effect::Copy(d.text.clone())),
            Message::Reopen => self.detail.clone().map(Effect::Reopen),
            Message::Delete => {
                self.confirm_delete = self.selected().is_some();
                None
            }
            Message::ConfirmDelete => {
                let confirmed = std::mem::take(&mut self.confirm_delete);
                self.selected()
                    .filter(|_| confirmed)
                    .map(|e| Effect::Delete(e.r))
            }
            Message::CancelDelete => {
                self.confirm_delete = false;
                None
            }
            Message::Close => Some(Effect::Close),
        }
    }

    fn submit_query(&mut self) -> Option<Effect> {
        if !self.matches.is_empty() {
            self.match_index = (self.match_index + 1) % self.matches.len();
            return self.jump_to_match();
        }
        let text = self.query.trim();
        if text.is_empty() {
            return None;
        }
        let (from, to) = self.day_bounds;
        Some(Effect::Search(SearchQuery {
            text: text.to_string(),
            kind: None,
            app: self.app_filter.clone(),
            workspace: None,
            from: Some(from),
            to: Some(to),
            limit: MATCH_LIMIT,
        }))
    }

    fn jump_to_match(&mut self) -> Option<Effect> {
        let ts = *self.matches.get(self.match_index)?;
        let index = nearest_entry(&self.entries, ts, None);
        self.move_cursor(index)
    }

    fn step_index(&self, delta: i32) -> Option<usize> {
        let candidates: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.passes_filter(&self.entries[i]))
            .collect();
        let current = self.cursor?;
        let position = candidates.iter().position(|&i| i >= current).unwrap_or(0);
        let next = position
            .saturating_add_signed(delta as isize)
            .min(candidates.len().saturating_sub(1));
        candidates.get(next).copied()
    }

    fn passes_filter(&self, entry: &TimelineEntry) -> bool {
        self.app_filter.as_ref().is_none_or(|app| *app == entry.app)
    }

    fn move_cursor(&mut self, index: Option<usize>) -> Option<Effect> {
        let index = index?;
        if self.cursor == Some(index) && self.detail.is_some() {
            return None;
        }
        self.cursor = Some(index);
        self.confirm_delete = false;
        self.detail = None;
        let r = self.entries.get(index)?.r;
        if self.image.as_ref().is_some_and(|(shown, _)| *shown != r) {
            self.image = None;
        }
        Some(Effect::Select(r))
    }

    pub fn view<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        let content: Element<'a, Message> = match &self.gate {
            Some(gate) => self.locked_view(gate),
            None => column![
                self.top_bar(),
                row![self.sidebar(), self.main_column(clock)]
                    .spacing(12)
                    .height(Length::Fill),
            ]
            .spacing(12)
            .into(),
        };
        container(content)
            .padding(14)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style::backdrop)
            .into()
    }

    fn locked_view<'a>(&'a self, gate: &'a Gate) -> Element<'a, Message> {
        let card = column![
            self.unlock.header(gate).map(Message::Unlock),
            self.unlock.body(gate).map(Message::Unlock),
        ]
        .spacing(10);
        container(
            container(card)
                .padding([12, 18])
                .width(480)
                .style(style::card),
        )
        .center(Length::Fill)
        .into()
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let input = text_input("Search the timeline", &self.query)
            .id(QUERY_INPUT)
            .on_input(Message::QueryChanged)
            .on_submit(Message::QuerySubmit)
            .padding([8, 12])
            .size(15)
            .style(style::boxed_input);
        let date = container(
            row![
                small_button(Icon::ChevronLeft, Message::PrevDay),
                icon(Icon::Calendar, 14.0, style::TEXT_MUTED),
                text(day_label(self.day)).size(13),
                small_button(Icon::ChevronRight, Message::NextDay),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([2, 6])
        .style(style::pill(style::TEXT));
        let lock = container(
            row![
                icon(Icon::LockOpen, 13.0, style::SUCCESS),
                text(
                    self.locks_in_secs
                        .map_or_else(|| "unlocked".to_string(), lock_countdown)
                )
                .size(13),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .style(style::pill(style::SUCCESS));
        let close = button(icon(Icon::Close, 16.0, style::TEXT_MUTED))
            .on_press(Message::Close)
            .padding(6)
            .style(style::ghost);
        row![input, date, lock, close]
            .spacing(10)
            .align_y(Alignment::Center)
            .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let apps = self.apps().into_iter().map(|app| {
            let active = self.app_filter.as_deref() == Some(app.as_str());
            let swatch = container(Space::new())
                .width(10)
                .height(10)
                .style(style::fill(style::app_colour(&app), 2.0));
            let label = text(app.clone()).size(13).color(if active {
                style::TEXT
            } else {
                style::TEXT_MUTED
            });
            button(row![swatch, label].spacing(8).align_y(Alignment::Center))
                .on_press(Message::PickApp(app))
                .padding([4, 6])
                .width(Length::Fill)
                .style(style::chip_flat(active))
                .into()
        });
        let workspaces = if self.workspaces.is_empty() {
            "none seen yet".to_string()
        } else {
            sorted_workspaces(&self.workspaces).join(" · ")
        };
        let never = self
            .never_recorded
            .iter()
            .map(|item| text(item.as_str()).size(13).color(style::SUCCESS).into());
        let content = column![
            section_title("APPS"),
            Column::with_children(apps).spacing(2),
            section_title("WORKSPACES"),
            text(workspaces).size(13).color(style::TEXT_MUTED),
            section_title("NEVER RECORDED"),
            Column::with_children(never).spacing(4),
        ]
        .spacing(8);
        container(scrollable(content).height(Length::Fill))
            .padding(12)
            .width(SIDEBAR_WIDTH)
            .height(Length::Fill)
            .style(style::panel)
            .into()
    }

    /// Apps of the day, most captured first.
    fn apps(&self) -> Vec<String> {
        let screens = self.entries.iter().filter(|e| e.kind == Kind::Screen);
        crate::search::rank_apps(screens.map(|e| e.app.as_str()))
    }

    fn main_column<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        column![self.moment(clock), self.lanes(clock)]
            .spacing(12)
            .width(Length::Fill)
            .into()
    }

    fn moment<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        let overlay = column![
            space::vertical(),
            row![self.meta_pill(clock), space::horizontal(), self.actions()]
                .spacing(8)
                .align_y(Alignment::End),
        ]
        .padding(12);
        container(stack![self.picture(), overlay])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style::panel)
            .into()
    }

    fn picture(&self) -> Element<'_, Message> {
        let placeholder = |line: String| -> Element<'_, Message> {
            container(text(line).size(14).color(style::TEXT_FAINT))
                .center(Length::Fill)
                .into()
        };
        let Some(entry) = self.selected() else {
            return placeholder("No captures on this day".into());
        };
        if entry.kind == Kind::Clipboard {
            let body = self.detail.as_ref().map_or("", |d| d.text.as_str());
            return container(
                column![
                    row![
                        icon(Icon::Clipboard, 16.0, style::CLIPBOARD_DOT),
                        text("Copied text").size(13).color(style::TEXT_MUTED),
                    ]
                    .spacing(8),
                    text(body).size(15).font(Font::MONOSPACE),
                ]
                .spacing(12),
            )
            .padding(24)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        }
        match &self.image {
            Some((r, handle)) if *r == entry.r => container(
                image(handle.clone())
                    .content_fit(iced::ContentFit::Contain)
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .padding(8)
            .into(),
            _ => placeholder("Loading screenshot".into()),
        }
    }

    fn meta_pill<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        let Some(entry) = self.selected() else {
            return Space::new().into();
        };
        let line = match &self.detail {
            Some(detail) => moment_meta(&detail.hit, detail.cwd.as_deref(), clock),
            None => format!("{} · {}", entry.app, clock_time(entry.ts, clock)),
        };
        let line = match &self.notice {
            Some(notice) => format!("{line}  ·  {notice}"),
            None => line,
        };
        container(text(line).size(13))
            .padding([6, 10])
            .style(style::fill(
                Color {
                    a: 0.85,
                    ..style::BACKDROP
                },
                6.0,
            ))
            .into()
    }

    fn actions(&self) -> Element<'_, Message> {
        if self.selected().is_none() {
            return Space::new().into();
        }
        if self.confirm_delete {
            return row![
                container(text("Delete this moment?").size(13))
                    .padding([6, 10])
                    .style(style::fill(
                        Color {
                            a: 0.85,
                            ..style::BACKDROP
                        },
                        6.0
                    )),
                action_button(Icon::Trash, "Delete", style::DANGER, Message::ConfirmDelete),
                action_button(
                    Icon::Close,
                    "Cancel",
                    style::SURFACE_RAISED,
                    Message::CancelDelete
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into();
        }
        row![
            action_button(
                Icon::Copy,
                "Copy text",
                style::ACCENT_SOFT,
                Message::CopyText
            ),
            action_button(Icon::Reopen, "Reopen", style::ACCENT_SOFT, Message::Reopen),
            action_button(Icon::Trash, "Delete", style::DANGER_SOFT, Message::Delete),
        ]
        .spacing(8)
        .into()
    }

    fn lanes<'a>(&'a self, clock: &'a Clock) -> Element<'a, Message> {
        let program = Lanes {
            segments: &self.segments,
            entries: &self.entries,
            range: axis_range(&self.entries, self.day_bounds),
            cursor: self.selected().map(|e| e.ts),
            matches: &self.matches,
            app_filter: self.app_filter.as_deref(),
            clock,
            on_scrub: Message::ScrubTo,
            on_step: Message::Step,
        };
        canvas(program)
            .width(Length::Fill)
            .height(lanes::HEIGHT)
            .into()
    }
}

/// Closest entry to `ts`, optionally only among one app's entries.
pub fn nearest_entry(entries: &[TimelineEntry], ts: Ts, app: Option<&str>) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| app.is_none_or(|app| e.app == app))
        .min_by_key(|(_, e)| (e.ts - ts).abs())
        .map(|(i, _)| i)
}

fn moment_meta(hit: &Hit, cwd: Option<&str>, clock: &Clock) -> String {
    let mut parts = vec![hit.app.clone()];
    if !hit.workspace.is_empty() {
        parts.push(format!("ws {}", hit.workspace));
    }
    parts.extend(
        hit.context
            .clone()
            .or_else(|| cwd.map(str::to_string))
            .filter(|c| !c.is_empty()),
    );
    parts.push(clock_time(hit.ts, clock));
    parts.join(" · ")
}

fn sorted_workspaces(workspaces: &BTreeSet<String>) -> Vec<&str> {
    let mut sorted: Vec<&str> = workspaces.iter().map(String::as_str).collect();
    sorted.sort_by_key(|w| (w.parse::<i64>().unwrap_or(i64::MAX), w.to_string()));
    sorted
}

fn section_title<'a>(title: &'a str) -> Element<'a, Message> {
    container(text(title).size(11).color(style::TEXT_FAINT))
        .padding(iced::Padding::ZERO.top(6))
        .into()
}

fn small_button<'a>(glyph: Icon, message: Message) -> Element<'a, Message> {
    button(icon(glyph, 14.0, style::TEXT_MUTED))
        .on_press(message)
        .padding(4)
        .style(style::ghost)
        .into()
}

fn action_button<'a>(
    glyph: Icon,
    label: &'a str,
    tone: Color,
    message: Message,
) -> Element<'a, Message> {
    button(
        row![icon(glyph, 14.0, style::TEXT), text(label).size(13)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(message)
    .padding([6, 12])
    .style(style::action(tone))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn loaded(target: Option<RecordRef>) -> (TimelineState, Clock) {
        let clock = fixtures::clock();
        let mut state = TimelineState::new(&clock, Vec::new());
        state.set_gate(None);
        state.show_day(clock.today(), target, &clock);
        state.set_entries(fixtures::day_entries());
        (state, clock)
    }

    #[test]
    fn nearest_entry_respects_app() {
        let entries = fixtures::day_entries();
        let mut sorted = entries.clone();
        sorted.sort_by_key(|e| e.ts);
        let probe = fixtures::at(27, 10, 0, 10);
        let any = nearest_entry(&sorted, probe, None).unwrap();
        assert_eq!(sorted[any].app, "firefox");
        let kitty = nearest_entry(&sorted, probe, Some("kitty")).unwrap();
        assert_eq!(sorted[kitty].ts, fixtures::at(27, 9, 59, 30));
        assert!(nearest_entry(&[], probe, None).is_none());
    }

    #[test]
    fn loading_a_day_selects_target_or_latest() {
        let target = fixtures::hits()[0].r;
        let (state, _) = loaded(Some(target));
        assert_eq!(state.selected().unwrap().r, target);
        let (state, _) = loaded(None);
        assert_eq!(state.selected().unwrap().ts, fixtures::at(27, 16, 59, 30));
    }

    #[test]
    fn stepping_and_scrubbing_move_the_cursor() {
        let (mut state, clock) = loaded(Some(fixtures::hits()[0].r));
        let Some(Effect::Select(_)) = state.update(Message::Step(1), &clock) else {
            panic!("expected a selection");
        };
        assert_eq!(state.selected().unwrap().ts, fixtures::at(27, 14, 32, 30));
        state.update(Message::ScrubTo(fixtures::at(27, 9, 25, 4)), &clock);
        assert_eq!(state.selected().unwrap().kind, Kind::Clipboard);
        state.update(Message::PickApp("neovim".into()), &clock);
        state.update(Message::Step(1), &clock);
        assert_eq!(state.selected().unwrap().app, "neovim");
    }

    #[test]
    fn delete_needs_confirmation() {
        let (mut state, clock) = loaded(None);
        let selected = state.selected().unwrap().r;
        assert!(state.update(Message::Delete, &clock).is_none());
        let Some(Effect::Delete(r)) = state.update(Message::ConfirmDelete, &clock) else {
            panic!("expected delete");
        };
        assert_eq!(r, selected);
        assert!(state.update(Message::ConfirmDelete, &clock).is_none());
    }

    #[test]
    fn next_day_stops_at_today() {
        let (mut state, clock) = loaded(None);
        assert!(state.update(Message::NextDay, &clock).is_none());
        let Some(Effect::LoadDay { from, to }) = state.update(Message::PrevDay, &clock) else {
            panic!("expected load");
        };
        assert_eq!(to - from, 24 * 3600 * 1000);
        assert_eq!(to, fixtures::at(27, 0, 0, 0));
    }
}
