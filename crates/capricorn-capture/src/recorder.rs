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
use crate::exclude::{Coverage, Snapshot, blackout, is_excluded, stable};
use crate::idle;
use crate::link::{DaemonLink, LinkEvent};
use crate::scheduler::{Decision, Input, Scheduler};
use crate::screencopy::capture_output;
use crate::wl;

const TICK: Duration = Duration::from_millis(250);

pub fn now_ms() -> Ts {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Ts)
}

struct Channels {
    idle: UnboundedReceiver<Input>,
    clip: UnboundedReceiver<ClipText>,
    hypr: UnboundedReceiver<HyprEvent>,
    link: UnboundedReceiver<LinkEvent>,
    died: UnboundedReceiver<anyhow::Error>,
}

pub async fn run() -> anyhow::Result<()> {
    let path = config::default_path().context("neither XDG_CONFIG_HOME nor HOME is set")?;
    let settings = config::load(&path)?;
    let hypr = Hypr::from_env()?;
    let conn = wl::connect()?;

    let (died_tx, died) = mpsc::unbounded_channel();
    let (idle_tx, idle) = mpsc::unbounded_channel();
    let (clip_tx, clip) = mpsc::unbounded_channel();
    let (hypr_tx, hypr_events) = mpsc::unbounded_channel();
    let (link_tx, link) = mpsc::unbounded_channel();

    let (settle, idle_after) = (settings.settle, settings.idle);
    let idle_conn = conn.clone();
    spawn_watcher("idle", died_tx.clone(), move || {
        idle::watch(&idle_conn, settle, idle_after, idle_tx)
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
    };
    info!("recording started");
    recorder
        .event_loop(Channels {
            idle,
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
    tx: UnboundedSender<HyprEvent>,
    died: UnboundedSender<anyhow::Error>,
) {
    let error = match hypr.events().await {
        Ok(events) => {
            let mut events = std::pin::pin!(events);
            while let Some(event) = events.next().await {
                if tx.send(event).is_err() {
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
}

impl Recorder {
    async fn event_loop(&mut self, mut ch: Channels) -> anyhow::Result<()> {
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.refresh_fullscreen().await;
        loop {
            tokio::select! {
                _ = ticks.tick() => self.on_input(Input::Tick).await,
                Some(input) = ch.idle.recv() => self.on_input(input).await,
                Some(event) = ch.hypr.recv() => self.on_hypr(event).await,
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

    async fn on_hypr(&mut self, event: HyprEvent) {
        match event {
            HyprEvent::Other => return,
            HyprEvent::WindowTitle => {}
            _ => self.refresh_fullscreen().await,
        }
        self.on_input(Input::HyprEvent).await;
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

    async fn refresh_fullscreen(&mut self) {
        match self.snapshot().await {
            Ok(snap) => {
                let excluded = fullscreen_paused(&snap, &self.settings.pause_fullscreen);
                self.on_input(Input::Fullscreen { excluded }).await;
            }
            Err(e) => warn!("reading Hyprland state: {e:#}"),
        }
    }

    fn report_state(&mut self) {
        let state = self.scheduler.state();
        if self.reported == Some(state) {
            return;
        }
        self.reported = self.link.send(CaptureMsg::Recorder(state)).then_some(state);
    }

    async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let (clients, monitors, active) = tokio::try_join!(
            self.hypr.clients(),
            self.hypr.monitors(),
            self.hypr.active_window()
        )?;
        Ok(Snapshot {
            clients,
            monitors,
            active,
        })
    }

    /// Grabs every output between two Hyprland reads and drops the whole set when anything
    /// that decides exclusions moved in between.
    async fn capture_outputs(&mut self) -> anyhow::Result<()> {
        let before = self.snapshot().await?;
        let grabs = self.grab(&before.monitors).await?;
        let after = self.snapshot().await?;
        if !stable(&before, &after, &self.settings.rules) {
            debug!("frame dropped: windows moved during capture");
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
        if snap
            .active
            .as_ref()
            .is_some_and(|c| is_excluded(c, &self.settings.rules))
        {
            return;
        }
        let entry = ClipboardEntry {
            ts: now_ms(),
            text: clip.text,
            source: focused_context(&snap, &self.settings.rules),
        };
        if !self.link.send(CaptureMsg::Clipboard(entry)) {
            debug!("clipboard entry dropped: daemon not connected or busy");
        }
    }
}
