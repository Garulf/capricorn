use std::sync::Mutex;
use std::time::Duration;

use capricorn_core::Secret;
use capricorn_core::proto::{
    ClientReq, ClientResp, DeleteScope, Hit, RecordDetail, RecordRef, Status, Ts, VaultState,
};
use iced::futures::SinkExt;
use iced::widget::{image, operation};
use iced::window::Id;
use iced::{Color, Element, Event, Subscription, Task, event, keyboard, window};
use iced_layershell::reexport::{
    Anchor, KeyboardInteractivity, Layer, NewLayerShellSettings, OutputOption,
};
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::to_layer_message;

use crate::client::{DaemonClient, Reply, Session};
use crate::control::{self, ControlCmd};
use crate::format::Clock;
use crate::keys::{Shortcut, shortcut};
use crate::search::{self, SearchState};
use crate::timeline::{self, TimelineState};
use crate::unlock::{Gate, PASSWORD_INPUT};
use crate::{reopen, style};

const DEBOUNCE: Duration = Duration::from_millis(60);

static CONTROL_LISTENER: Mutex<Option<std::os::unix::net::UnixListener>> = Mutex::new(None);

pub fn run(
    client: DaemonClient,
    never_recorded: Vec<String>,
    listener: std::os::unix::net::UnixListener,
) -> Result<(), iced_layershell::Error> {
    *CONTROL_LISTENER.lock().expect("listener lock") = Some(listener);
    iced_layershell::daemon(
        move || App::boot(client.clone(), never_recorded.clone()),
        "capricorn-ui",
        App::update,
        App::view,
    )
    .theme(|_: &App, _: Id| Some(style::theme()))
    .style(|_: &App, _: &iced::Theme| iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: style::TEXT,
    })
    .subscription(App::subscription)
    .settings(Settings {
        layer_settings: LayerShellSettings {
            start_mode: StartMode::Background,
            ..LayerShellSettings::default()
        },
        ..Settings::default()
    })
    .run()
}

type Response = Result<Reply, String>;

#[to_layer_message(multi)]
#[derive(Debug, Clone)]
pub enum Message {
    Control(ControlCmd),
    Search(search::Message),
    Timeline(timeline::Message),
    Opened(Id),
    Closed(Id),
    Key {
        window: Id,
        shortcut: Shortcut,
        captured: bool,
    },
    Tick,
    Status(Response),
    Unlocked(Response),
    Hits(Response),
    Thumb(RecordRef, Response),
    Day(Response),
    Detail(Response),
    Picture(RecordRef, Response),
    Matches(Response),
    Deleted(Response),
    ReopenDetail(Response),
    Reopened(Result<(), String>),
    CopyFallback(Secret),
    Copied,
}

pub struct App {
    client: DaemonClient,
    session: Session,
    clock: Clock,
    search: SearchState,
    timeline: TimelineState,
    search_surface: Option<(Id, u32)>,
    timeline_surface: Option<Id>,
    /// Wall-clock milliseconds, so a suspend in between still counts down.
    locks_at: Option<Ts>,
}

impl App {
    fn boot(client: DaemonClient, never_recorded: Vec<String>) -> (Self, Task<Message>) {
        let clock = Clock::system();
        let mut app = Self {
            client,
            session: Session::default(),
            timeline: TimelineState::new(&clock, never_recorded),
            clock,
            search: SearchState::default(),
            search_surface: None,
            timeline_surface: None,
            locks_at: None,
        };
        app.search.set_gate(Some(Gate::Connecting));
        let task = app.request(ClientReq::Status, Message::Status);
        (app, task)
    }

    fn request(
        &self,
        req: ClientReq,
        on_reply: impl FnOnce(Response) -> Message + Send + 'static,
    ) -> Task<Message> {
        let client = self.client.clone();
        Task::perform(async move { client.request(req).await }, on_reply)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        self.clock.now = jiff::Timestamp::now().as_millisecond();
        match message {
            Message::Control(cmd) => self.control(cmd),
            Message::Search(message) => {
                let effect = self.search.update(message, &self.clock);
                let task = effect.map_or_else(Task::none, |e| self.search_effect(e));
                Task::batch([task, self.fit_search()])
            }
            Message::Timeline(message) => {
                let effect = self.timeline.update(message, &self.clock);
                effect.map_or_else(Task::none, |e| self.timeline_effect(e))
            }
            Message::Opened(id) => self.focus_for(id),
            Message::Closed(id) => {
                self.forget_surface(id);
                Task::none()
            }
            Message::Key {
                window,
                shortcut,
                captured,
            } => self.key(window, shortcut, captured),
            Message::Tick => self.tick(),
            Message::Status(response) => self.status(response),
            Message::Unlocked(response) => self.unlocked(response),
            Message::Hits(response) => self.hits(response),
            Message::Thumb(r, response) => {
                if let Some(ClientResp::Image(bytes)) = self.accept(response) {
                    self.search.set_thumb(r, image::Handle::from_bytes(bytes));
                }
                Task::none()
            }
            Message::Day(response) => match self.accept(response) {
                Some(ClientResp::Timeline(entries)) => self
                    .timeline
                    .set_entries(entries)
                    .map_or_else(Task::none, |e| self.timeline_effect(e)),
                other => self.report_error(other),
            },
            Message::Detail(response) => {
                if let Some(ClientResp::Detail(detail)) = self.accept(response) {
                    self.timeline.set_detail(detail);
                }
                Task::none()
            }
            Message::Picture(r, response) => {
                if let Some(ClientResp::Image(bytes)) = self.accept(response) {
                    self.timeline.set_image(r, image::Handle::from_bytes(bytes));
                }
                Task::none()
            }
            Message::Matches(response) => match self.accept(response) {
                Some(ClientResp::Hits(hits)) => self
                    .timeline
                    .set_matches(&hits)
                    .map_or_else(Task::none, |e| self.timeline_effect(e)),
                other => self.report_error(other),
            },
            Message::Deleted(response) => match self.accept(response) {
                Some(ClientResp::Ok) => {
                    self.timeline.set_notice(Some("Deleted".into()));
                    let effect = self.timeline.reload();
                    self.timeline_effect(effect)
                }
                other => self.report_error(other),
            },
            Message::ReopenDetail(response) => match self.accept(response) {
                Some(ClientResp::Detail(detail)) => reopen_task(detail),
                other => self.report_error(other),
            },
            Message::Reopened(result) => {
                if let Err(error) = result {
                    tracing::warn!("reopen failed: {error}");
                    self.notify(Some(format!("Reopen failed: {error}")));
                }
                Task::none()
            }
            Message::CopyFallback(text) => {
                self.notify(Some("Copied".into()));
                iced::clipboard::write(text.expose().to_string())
            }
            Message::Copied => {
                self.notify(Some("Copied".into()));
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn view(&self, id: Id) -> Element<'_, Message> {
        if self.search_surface.is_some_and(|(s, _)| s == id) {
            return self.search.view(&self.clock).map(Message::Search);
        }
        if self.timeline_surface == Some(id) {
            return self.timeline.view(&self.clock).map(Message::Timeline);
        }
        iced::widget::Space::new().into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            Subscription::run(control_commands).map(Message::Control),
            event::listen_with(key_event),
            window::open_events().map(Message::Opened),
            window::close_events().map(Message::Closed),
        ];
        if self.search_surface.is_some() || self.timeline_surface.is_some() {
            subscriptions.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        Subscription::batch(subscriptions)
    }

    fn control(&mut self, cmd: ControlCmd) -> Task<Message> {
        match cmd {
            ControlCmd::Search if self.search_surface.is_some() => self.close_search(),
            ControlCmd::Search => Task::batch([self.close_timeline(), self.open_search()]),
            ControlCmd::Timeline if self.timeline_surface.is_some() => self.close_timeline(),
            ControlCmd::Timeline => {
                let today = self.clock.today();
                Task::batch([self.close_search(), self.open_timeline(today, None)])
            }
            ControlCmd::Hide => Task::batch([self.close_search(), self.close_timeline()]),
        }
    }

    fn open_search(&mut self) -> Task<Message> {
        self.search.reset();
        let id = Id::unique();
        let height = self.search.height();
        self.search_surface = Some((id, height));
        let settings = NewLayerShellSettings {
            size: Some((search::WIDTH, height)),
            layer: Layer::Overlay,
            anchor: Anchor::empty(),
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            output_option: OutputOption::Active,
            namespace: Some("capricorn-search".into()),
            ..NewLayerShellSettings::default()
        };
        Task::batch([
            Task::done(Message::NewLayerShell { settings, id }),
            self.request(ClientReq::Status, Message::Status),
        ])
    }

    fn open_timeline(
        &mut self,
        day: jiff::civil::Date,
        target: Option<RecordRef>,
    ) -> Task<Message> {
        let id = Id::unique();
        self.timeline_surface = Some(id);
        let margin = timeline::MARGIN;
        let settings = NewLayerShellSettings {
            size: None,
            layer: Layer::Top,
            anchor: Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right,
            margin: Some((margin, margin, margin, margin)),
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            output_option: OutputOption::Active,
            namespace: Some("capricorn-timeline".into()),
            ..NewLayerShellSettings::default()
        };
        let load = self.timeline.show_day(day, target, &self.clock);
        let load = if self.session.is_unlocked() {
            self.timeline_effect(load)
        } else {
            Task::none()
        };
        Task::batch([
            Task::done(Message::NewLayerShell { settings, id }),
            self.request(ClientReq::Status, Message::Status),
            load,
        ])
    }

    fn close_search(&mut self) -> Task<Message> {
        match self.search_surface.take() {
            Some((id, _)) => window::close(id),
            None => Task::none(),
        }
    }

    fn close_timeline(&mut self) -> Task<Message> {
        match self.timeline_surface.take() {
            Some(id) => window::close(id),
            None => Task::none(),
        }
    }

    fn forget_surface(&mut self, id: Id) {
        if self.search_surface.is_some_and(|(s, _)| s == id) {
            self.search_surface = None;
        }
        if self.timeline_surface == Some(id) {
            self.timeline_surface = None;
        }
    }

    fn fit_search(&mut self) -> Task<Message> {
        let Some((id, height)) = self.search_surface else {
            return Task::none();
        };
        let wanted = self.search.height();
        if wanted == height {
            return Task::none();
        }
        self.search_surface = Some((id, wanted));
        Task::done(Message::SizeChange {
            id,
            size: (search::WIDTH, wanted),
        })
    }

    fn focus_for(&self, id: Id) -> Task<Message> {
        if self.search_surface.is_some_and(|(s, _)| s == id) {
            if self.search.is_locked() {
                return operation::focus(PASSWORD_INPUT);
            }
            return operation::focus(search::QUERY_INPUT);
        }
        if self.timeline_surface == Some(id) && self.timeline.is_locked() {
            return operation::focus(PASSWORD_INPUT);
        }
        Task::none()
    }

    fn key(&mut self, window: Id, shortcut: Shortcut, captured: bool) -> Task<Message> {
        if self.search_surface.is_some_and(|(s, _)| s == window) {
            let message = match shortcut {
                Shortcut::Close => search::Message::Close,
                Shortcut::Up => search::Message::Move(-1),
                Shortcut::Down => search::Message::Move(1),
                Shortcut::Copy => search::Message::Copy,
                Shortcut::Reopen => search::Message::Reopen,
                _ => return Task::none(),
            };
            return self.update(Message::Search(message));
        }
        if self.timeline_surface == Some(window) {
            let message = match shortcut {
                Shortcut::Close => timeline::Message::Close,
                Shortcut::Left if !captured => timeline::Message::Step(-1),
                Shortcut::Right if !captured => timeline::Message::Step(1),
                Shortcut::PrevDay => timeline::Message::PrevDay,
                Shortcut::NextDay => timeline::Message::NextDay,
                Shortcut::Copy if !captured => timeline::Message::CopyText,
                Shortcut::Reopen => timeline::Message::Reopen,
                _ => return Task::none(),
            };
            return self.update(Message::Timeline(message));
        }
        Task::none()
    }

    /// Runs only while a surface is shown. Asking every tick is how the UI learns that
    /// the daemon locked, since the daemon never speaks unasked.
    fn tick(&mut self) -> Task<Message> {
        let remaining = self.locks_at.map(|at| secs_until(at, self.clock.now));
        self.search.set_locks_in(remaining);
        self.timeline.set_locks_in(remaining);
        self.request(ClientReq::Status, Message::Status)
    }

    fn status(&mut self, response: Response) -> Task<Message> {
        let reply = match response {
            Ok(reply) => reply,
            Err(error) => {
                self.lock_ui(Gate::Unreachable(error));
                return self.fit_search();
            }
        };
        let ClientResp::Status(Status { vault, session, .. }) = reply.resp else {
            return Task::none();
        };
        let was_locked = self.locked_views();
        match vault {
            VaultState::Uninitialized => self.lock_ui(Gate::Uninitialized),
            VaultState::Unlocked { locks_in_secs } if session && self.session.accepts(&reply) => {
                let locks_in_ms =
                    Ts::try_from(locks_in_secs.saturating_mul(1000)).unwrap_or(Ts::MAX);
                self.locks_at = Some(self.clock.now.saturating_add(locks_in_ms));
                self.search.set_locks_in(Some(locks_in_secs));
                self.timeline.set_locks_in(Some(locks_in_secs));
            }
            _ => self.lock_ui(Gate::Locked),
        }
        // Refocusing moves the cursor to the end of the field, so only when the view
        // switched; status arrives every second while a surface is shown.
        let refocus = if self.locked_views() == was_locked {
            Task::none()
        } else {
            self.refocus()
        };
        Task::batch([self.fit_search(), refocus])
    }

    fn locked_views(&self) -> (bool, bool) {
        (self.search.is_locked(), self.timeline.is_locked())
    }

    fn lock_ui(&mut self, gate: Gate) {
        self.session.lock();
        self.locks_at = None;
        self.search.set_gate(Some(gate.clone()));
        self.timeline.set_gate(Some(gate));
    }

    fn refocus(&self) -> Task<Message> {
        let surfaces = [self.search_surface.map(|(id, _)| id), self.timeline_surface];
        Task::batch(surfaces.into_iter().flatten().map(|id| self.focus_for(id)))
    }

    fn unlocked(&mut self, response: Response) -> Task<Message> {
        match response {
            Ok(Reply {
                session,
                resp: ClientResp::Ok,
            }) => {
                self.session.unlocked_on(session);
                self.search.unlock.succeeded();
                self.timeline.unlock.succeeded();
                self.search.set_gate(None);
                self.timeline.set_gate(None);
                let reload = self.timeline.reload();
                let reload = if self.timeline_surface.is_some() {
                    self.timeline_effect(reload)
                } else {
                    Task::none()
                };
                Task::batch([
                    self.request(ClientReq::Status, Message::Status),
                    self.fit_search(),
                    self.refocus(),
                    reload,
                ])
            }
            Ok(Reply { resp, .. }) => {
                let error = match resp {
                    ClientResp::Err(error) => error,
                    other => format!("unexpected reply: {other:?}"),
                };
                self.search.unlock.failed(error.clone());
                self.timeline.unlock.failed(error);
                Task::none()
            }
            Err(error) => {
                self.search.unlock.failed(error.clone());
                self.timeline.unlock.failed(error);
                Task::none()
            }
        }
    }

    fn hits(&mut self, response: Response) -> Task<Message> {
        let hits = match self.accept(response) {
            Some(ClientResp::Hits(hits)) => hits,
            other => return Task::batch([self.report_error(other), self.fit_search()]),
        };
        let missing = self.search.set_hits(hits);
        let fit = self.fit_search();
        let thumbs = missing.into_iter().map(|r| {
            self.request(ClientReq::Image { r, thumb: true }, move |resp| {
                Message::Thumb(r, resp)
            })
        });
        Task::batch(thumbs.chain([fit]))
    }

    /// The response body when it belongs to this UI's unlocked session.
    fn accept(&mut self, response: Response) -> Option<ClientResp> {
        match response {
            Ok(reply) if self.session.accepts(&reply) => Some(reply.resp),
            Ok(_) => {
                self.lock_ui(Gate::Locked);
                None
            }
            Err(error) => {
                self.lock_ui(Gate::Unreachable(error));
                None
            }
        }
    }

    fn report_error(&mut self, resp: Option<ClientResp>) -> Task<Message> {
        if let Some(ClientResp::Err(error)) = resp {
            self.notify(Some(error));
        }
        Task::batch([self.fit_search(), self.refocus()])
    }

    fn notify(&mut self, notice: Option<String>) {
        self.search.set_notice(notice.clone());
        self.timeline.set_notice(notice);
    }

    fn search_effect(&mut self, effect: search::Effect) -> Task<Message> {
        match effect {
            search::Effect::Unlock(password) => {
                self.request(ClientReq::Unlock { password }, Message::Unlocked)
            }
            search::Effect::Debounce(generation) => {
                Task::perform(tokio::time::sleep(DEBOUNCE), move |_| {
                    Message::Search(search::Message::Debounced(generation))
                })
            }
            search::Effect::Search(query) => self.request(ClientReq::Search(query), Message::Hits),
            search::Effect::Open(hit) => self.open_hit(&hit),
            search::Effect::Copy(text) => copy_task(text),
            search::Effect::Reopen(hit) => Task::batch([
                self.request(ClientReq::Detail(hit.r), Message::ReopenDetail),
                self.close_search(),
            ]),
            search::Effect::Close => self.close_search(),
        }
    }

    fn open_hit(&mut self, hit: &Hit) -> Task<Message> {
        let day = self.clock.zoned(hit.ts).date();
        let close = self.close_search();
        let open = match self.timeline_surface {
            Some(_) => {
                let load = self.timeline.show_day(day, Some(hit.r), &self.clock);
                self.timeline_effect(load)
            }
            None => self.open_timeline(day, Some(hit.r)),
        };
        Task::batch([close, open])
    }

    fn timeline_effect(&mut self, effect: timeline::Effect) -> Task<Message> {
        match effect {
            timeline::Effect::Unlock(password) => {
                self.request(ClientReq::Unlock { password }, Message::Unlocked)
            }
            timeline::Effect::LoadDay { from, to } => {
                self.request(ClientReq::Timeline { from, to }, Message::Day)
            }
            timeline::Effect::Select(r) => Task::batch([
                self.request(ClientReq::Detail(r), Message::Detail),
                self.request(ClientReq::Image { r, thumb: false }, move |resp| {
                    Message::Picture(r, resp)
                }),
            ]),
            timeline::Effect::Search(query) => {
                self.request(ClientReq::Search(query), Message::Matches)
            }
            timeline::Effect::Copy(text) => copy_task(text),
            timeline::Effect::Reopen(detail) => reopen_task(detail),
            timeline::Effect::Delete(r) => {
                self.request(ClientReq::Delete(DeleteScope::One(r)), Message::Deleted)
            }
            timeline::Effect::Close => self.close_timeline(),
        }
    }
}

fn secs_until(at: Ts, now: Ts) -> u64 {
    u64::try_from(at.saturating_sub(now).max(0) / 1000).unwrap_or(0)
}

fn reopen_task(detail: RecordDetail) -> Task<Message> {
    Task::perform(reopen::reopen(detail), Message::Reopened)
}

fn copy_task(text: String) -> Task<Message> {
    let text = Secret::new(text);
    Task::perform(wl_copy(text.clone()), move |result| match result {
        Ok(()) => Message::Copied,
        Err(_) => Message::CopyFallback(text.clone()),
    })
}

/// `wl-copy` keeps serving the selection after this process closes the surface.
async fn wl_copy(text: Secret) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin.write_all(text.expose().as_bytes()).await?;
    drop(stdin);
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "wl-copy exited with {status}"
        )))
    }
}

fn key_event(event: Event, status: event::Status, window: Id) -> Option<Message> {
    let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
        return None;
    };
    Some(Message::Key {
        window,
        shortcut: shortcut(&key, modifiers)?,
        captured: status == event::Status::Captured,
    })
}

fn control_commands() -> impl iced::futures::Stream<Item = ControlCmd> {
    iced::stream::channel(8, async |mut output| {
        let listener = CONTROL_LISTENER.lock().expect("listener lock").take();
        let Some(listener) = listener.and_then(|l| tokio::net::UnixListener::from_std(l).ok())
        else {
            return std::future::pending().await;
        };
        let (sender, mut commands) = tokio::sync::mpsc::channel(8);
        tokio::spawn(control::serve(listener, sender));
        while let Some(cmd) = commands.recv().await {
            if output.send(cmd).await.is_err() {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn status_reply(session: u64, vault: VaultState, holds_session: bool) -> Response {
        Ok(Reply {
            session,
            resp: ClientResp::Status(Status {
                vault,
                recorder: None,
                pending_index: 0,
                session: holds_session,
            }),
        })
    }

    const UNLOCKED: VaultState = VaultState::Unlocked { locks_in_secs: 600 };

    /// An app this UI unlocked on connection 1, with a search hit and a timeline day
    /// on screen.
    fn unlocked_app() -> App {
        let client = DaemonClient::new("/nonexistent/client.sock".into());
        let (mut app, _) = App::boot(client, Vec::new());
        app.clock = fixtures::clock();
        app.session.unlocked_on(1);
        app.search.set_gate(None);
        app.search.set_hits(fixtures::hits());
        app.timeline.set_gate(None);
        let _ = app.timeline.show_day(app.clock.today(), None, &app.clock);
        let _ = app.timeline.set_entries(fixtures::day_entries());
        let _ = app.status(status_reply(1, UNLOCKED, true));
        assert!(app.search.selected_hit().is_some());
        assert!(app.timeline.selected().is_some());
        app
    }

    fn assert_cleared(app: &App) {
        assert!(!app.session.is_unlocked());
        assert!(app.search.is_locked() && app.timeline.is_locked());
        assert!(app.search.selected_hit().is_none());
        assert!(app.timeline.selected().is_none());
        assert_eq!(app.locks_at, None);
    }

    #[test]
    fn unlocked_vault_without_this_connections_session_clears_the_ui() {
        let mut app = unlocked_app();
        let _ = app.status(status_reply(1, UNLOCKED, false));
        assert_cleared(&app);
    }

    #[test]
    fn locked_vault_clears_the_ui() {
        let mut app = unlocked_app();
        let _ = app.status(status_reply(1, VaultState::Locked, false));
        assert_cleared(&app);
    }

    #[test]
    fn countdown_follows_the_wall_clock() {
        let mut app = unlocked_app();
        assert_eq!(app.locks_at, Some(app.clock.now + 600_000));
        app.clock.now += 3_600_000;
        assert_eq!(secs_until(app.locks_at.unwrap(), app.clock.now), 0);
        assert_eq!(secs_until(10_500, 1_000), 9);
    }
}
