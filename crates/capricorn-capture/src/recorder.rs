use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use capricorn_core::proto::{CaptureCtl, CaptureMsg, ClipboardEntry, Frame, RecorderState, Ts};
use capricorn_hypr::{Event as HyprEvent, Hypr, Monitor};
use futures::StreamExt;
use image::RgbaImage;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use wayland_client::Connection;

use crate::clipboard::{self, ClipText};
use crate::config::{self, Settings};
use crate::context::{focused_context, fullscreen_paused, is_terminal, window_context};
use crate::cwd::foreground_cwd;
use crate::exclude::{
    Coverage, ExclusionMemory, Snapshot, blackout, focus_is_excluded, is_excluded, stable,
};
use crate::focus::{CLIPBOARD_LOOKBACK, FocusHistory, event_window_excluded};
use crate::idle;
use crate::link::{DaemonLink, LinkEvent};
use crate::lock;
use crate::scheduler::{Decision, Input, Scheduler};
use crate::screencopy::capture_output;
use crate::wl;

const TICK: Duration = Duration::from_millis(250);
/// Held back so focus changes that happened before the copy, but were read after it,
/// are in the focus history when the entry is judged.
const CLIPBOARD_HOLD: Duration = Duration::from_millis(500);

pub fn now_ms() -> Ts {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Ts)
}

struct Channels {
    inputs: UnboundedReceiver<Input>,
    clip: UnboundedReceiver<ClipText>,
    hypr: UnboundedReceiver<(Instant, HyprEvent)>,
    link: UnboundedReceiver<LinkEvent>,
    died: UnboundedReceiver<anyhow::Error>,
}

pub async fn run() -> anyhow::Result<()> {
    let path = config::default_path().context("neither XDG_CONFIG_HOME nor HOME is set")?;
    let settings = config::load(&path)?;
    let hypr = Hypr::from_env()?;
    let conn = wl::connect()?;

    let (died_tx, died) = mpsc::unbounded_channel();
    let (input_tx, inputs) = mpsc::unbounded_channel();
    let (clip_tx, clip) = mpsc::unbounded_channel();
    let (hypr_tx, hypr_events) = mpsc::unbounded_channel();
    let (link_tx, link) = mpsc::unbounded_channel();

    let (settle, idle_after) = (settings.settle, settings.idle);
    tokio::spawn(lock::watch(input_tx.clone()));
    let idle_conn = conn.clone();
    spawn_watcher("idle", died_tx.clone(), move || {
        idle::watch(&idle_conn, settle, idle_after, input_tx)
    });
    let clip_conn = conn.clone();
    spawn_watcher("clipboard", died_tx.clone(), move || {
        clipboard::watch(clip_conn, clip_tx)
    });
    tokio::spawn(forward_hypr_events(hypr.clone(), hypr_tx, died_tx));

    let mut recorder = Recorder {
        scheduler: Scheduler::new(settings.floor, settings.settle, Instant::now()),
        link: DaemonLink::spawn(link_tx),
        hypr,
        conn,
        settings,
        reported: None,
        exclusions: ExclusionMemory::default(),
        focus: FocusHistory::default(),
        held_clips: VecDeque::new(),
    };
    info!("recording started");
    recorder
        .event_loop(Channels {
            inputs,
            clip,
            hypr: hypr_events,
            link,
            died,
        })
        .await
}

fn spawn_watcher(
    name: &'static str,
    died: UnboundedSender<anyhow::Error>,
    watch: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) {
    std::thread::spawn(move || {
        let error = match watch() {
            Ok(()) => anyhow!("{name} watcher stopped"),
            Err(e) => e.context(format!("{name} watcher failed")),
        };
        let _ = died.send(error);
    });
}

async fn forward_hypr_events(
    hypr: Hypr,
    tx: UnboundedSender<(Instant, HyprEvent)>,
    died: UnboundedSender<anyhow::Error>,
) {
    let error = match hypr.events().await {
        Ok(events) => {
            let mut events = std::pin::pin!(events);
            while let Some(event) = events.next().await {
                if tx.send((Instant::now(), event)).is_err() {
                    return;
                }
            }
            anyhow!("Hyprland event socket closed")
        }
        Err(e) => anyhow!(e).context("subscribing to Hyprland events"),
    };
    let _ = died.send(error);
}

struct Recorder {
    scheduler: Scheduler,
    link: DaemonLink,
    hypr: Hypr,
    conn: Connection,
    settings: Settings,
    reported: Option<RecorderState>,
    exclusions: ExclusionMemory,
    focus: FocusHistory,
    held_clips: VecDeque<HeldClip>,
}

struct HeldClip {
    entry: ClipboardEntry,
    seen_at: Instant,
}

impl Recorder {
    async fn event_loop(&mut self, mut ch: Channels) -> anyhow::Result<()> {
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.refresh_fullscreen().await;
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    self.release_clips();
                    self.on_input(Input::Tick).await;
                }
                Some(input) = ch.inputs.recv() => self.on_input(input).await,
                Some((at, event)) = ch.hypr.recv() => self.on_hypr(at, event).await,
                Some(clip) = ch.clip.recv() => self.on_clip(clip).await,
                Some(event) = ch.link.recv() => self.on_link(event).await,
                Some(error) = ch.died.recv() => return Err(error),
                else => bail!("all event sources closed"),
            }
        }
    }

    async fn on_input(&mut self, input: Input) {
        let decision = self.scheduler.on(input, Instant::now());
        self.report_state();
        if decision == Decision::Capture
            && let Err(e) = self.capture_outputs().await
        {
            warn!("capture skipped: {e:#}");
        }
    }

    /// Every event but a title change may start an animation, including ones not parsed
    /// further such as `activespecial` or `changefloatingmode`.
    async fn on_hypr(&mut self, at: Instant, event: HyprEvent) {
        match event {
            HyprEvent::Versioned => {}
            HyprEvent::WindowTitle => {
                self.refresh_focus().await;
                self.on_input(Input::TitleChanged).await;
            }
            HyprEvent::ActiveWindow { class, title } => {
                let snap = self.refresh_fullscreen().await;
                let excluded =
                    event_window_excluded(&class, &title, snap.as_ref(), &self.settings.rules);
                self.focus.record(at, excluded);
                self.on_input(Input::HyprEvent).await;
            }
            _ => {
                self.refresh_fullscreen().await;
                self.on_input(Input::HyprEvent).await;
            }
        }
    }

    async fn on_link(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Connected => {
                self.reported = None;
                self.report_state();
            }
            LinkEvent::Ctl(CaptureCtl::Pause) => self.on_input(Input::Paused(true)).await,
            LinkEvent::Ctl(CaptureCtl::Resume) => self.on_input(Input::Paused(false)).await,
        }
    }

    async fn refresh_fullscreen(&mut self) -> Option<Snapshot> {
        match self.snapshot().await {
            Ok(snap) => {
                let excluded = fullscreen_paused(&snap, &self.settings.pause_fullscreen);
                self.on_input(Input::Fullscreen { excluded }).await;
                Some(snap)
            }
            Err(e) => {
                warn!("reading Hyprland state: {e:#}");
                None
            }
        }
    }

    /// Cheaper than a full snapshot, for events as frequent as title changes.
    async fn refresh_focus(&mut self) {
        let at = Instant::now();
        match self.hypr.active_window().await {
            Ok(active) => {
                let excluded = active
                    .as_ref()
                    .is_some_and(|c| is_excluded(c, &self.settings.rules));
                self.focus.record(at, excluded);
            }
            Err(e) => {
                warn!("reading the focused window: {e:#}");
                self.focus.record(at, true);
            }
        }
    }

    fn report_state(&mut self) {
        let state = self.scheduler.state();
        if self.reported == Some(state) {
            return;
        }
        self.reported = self.link.send(CaptureMsg::Recorder(state)).then_some(state);
    }

    /// Every snapshot also feeds the focus history and the memory of excluded windows.
    async fn snapshot(&mut self) -> anyhow::Result<Snapshot> {
        let (clients, monitors, active) = tokio::try_join!(
            self.hypr.clients(),
            self.hypr.monitors(),
            self.hypr.active_window()
        )?;
        let snap = Snapshot {
            clients,
            monitors,
            active,
        };
        let now = Instant::now();
        let rules = &self.settings.rules;
        self.exclusions.observe(&snap, rules, now);
        self.focus.record_snapshot(&snap, rules, now);
        Ok(snap)
    }

    /// Grabs every output between two Hyprland reads and drops the whole set when anything
    /// that decides exclusions moved in between, while an excluded window has focus, or
    /// while one may still be animating out of view.
    async fn capture_outputs(&mut self) -> anyhow::Result<()> {
        let before = self.snapshot().await?;
        if let Some(reason) = self.unsafe_to_capture(&before) {
            debug!("capture skipped: {reason}");
            return Ok(());
        }
        let grabs = self.grab(&before.monitors).await?;
        let after = self.snapshot().await?;
        if !stable(&before, &after, &self.settings.rules) {
            debug!("frame dropped: windows moved during capture");
            return Ok(());
        }
        if let Some(reason) = self.unsafe_to_capture(&after) {
            debug!("frame dropped: {reason}");
            return Ok(());
        }
        let visible = after.visible_clients();
        let ts = now_ms();
        for (mon, grab) in after.monitors.iter().zip(grabs) {
            match grab {
                Ok(mut img) => {
                    if blackout(&mut img, mon, &visible, &self.settings.rules) == Coverage::Full {
                        continue;
                    }
                    self.send_frame(ts, &after, mon, img);
                }
                Err(e) => warn!("grabbing {}: {e:#}", mon.name),
            }
        }
        Ok(())
    }

    fn unsafe_to_capture(&self, snap: &Snapshot) -> Option<&'static str> {
        if focus_is_excluded(snap, &self.settings.rules) {
            Some("an excluded window has focus")
        } else if self.exclusions.unsettled(Instant::now()) {
            Some("an excluded window was just hidden or moved")
        } else {
            None
        }
    }

    async fn grab(&self, monitors: &[Monitor]) -> anyhow::Result<Vec<anyhow::Result<RgbaImage>>> {
        let conn = self.conn.clone();
        let names: Vec<String> = monitors.iter().map(|m| m.name.clone()).collect();
        let grabs = tokio::task::spawn_blocking(move || {
            names
                .iter()
                .map(|name| capture_output(&conn, name))
                .collect()
        });
        Ok(grabs.await?)
    }

    fn send_frame(&self, ts: Ts, snap: &Snapshot, mon: &Monitor, img: RgbaImage) {
        let mut window = window_context(snap, mon, &self.settings.rules);
        if is_terminal(&window.class) {
            window.cwd = window.pid.and_then(foreground_cwd);
        }
        let frame = Frame {
            ts,
            output: mon.name.clone(),
            width: img.width(),
            height: img.height(),
            rgba: img.into_raw(),
            window,
        };
        if !self.link.send(CaptureMsg::Frame(frame)) {
            debug!("frame dropped: daemon not connected or busy");
        }
    }

    async fn on_clip(&mut self, clip: ClipText) {
        if self.scheduler.state() != RecorderState::Recording {
            return;
        }
        let snap = match self.snapshot().await {
            Ok(snap) => snap,
            Err(e) => {
                warn!("clipboard entry dropped, cannot read focus: {e:#}");
                return;
            }
        };
        if focus_is_excluded(&snap, &self.settings.rules) {
            return;
        }
        let entry = ClipboardEntry {
            ts: now_ms(),
            text: clip.text,
            source: focused_context(&snap, &self.settings.rules),
        };
        self.held_clips.push_back(HeldClip {
            entry,
            seen_at: clip.seen_at,
        });
    }

    /// Sends held entries once `CLIPBOARD_HOLD` has passed, unless an excluded window had
    /// focus within `CLIPBOARD_LOOKBACK` of the copy or since.
    fn release_clips(&mut self) {
        let now = Instant::now();
        while let Some(held) = self.held_clips.front() {
            if now.saturating_duration_since(held.seen_at) < CLIPBOARD_HOLD {
                return;
            }
            let Some(held) = self.held_clips.pop_front() else {
                return;
            };
            if self.scheduler.state() != RecorderState::Recording {
                continue;
            }
            if self.focus.excluded_since_lookback(held.seen_at) {
                debug!(
                    "clipboard entry dropped: an excluded window had focus within {CLIPBOARD_LOOKBACK:?}"
                );
                continue;
            }
            if !self.link.send(CaptureMsg::Clipboard(held.entry)) {
                debug!("clipboard entry dropped: daemon not connected or busy");
            }
        }
    }
}
