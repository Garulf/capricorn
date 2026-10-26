use std::time::Duration;

use anyhow::{Context, anyhow};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

use crate::scheduler::Input;
use crate::wl;

#[derive(Debug, Clone, Copy)]
enum Timer {
    Settle,
    Idle,
}

struct IdleState {
    tx: UnboundedSender<Input>,
}

/// Reports `Settled`/`Idle` after `settle`/`idle` without input and `Active` when input
/// resumes. Blocks until the connection fails or the receiver is gone.
pub fn watch(
    conn: &Connection,
    settle: Duration,
    idle: Duration,
    tx: UnboundedSender<Input>,
) -> anyhow::Result<()> {
    let (globals, mut queue) = registry_queue_init::<IdleState>(conn)?;
    let qh = queue.handle();
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .context("ext_idle_notifier_v1")?;
    let seat: WlSeat = wl::bind_all(&globals, &qh, 1)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no wl_seat"))?;

    let _settle = notification(&notifier, &seat, &qh, settle, Timer::Settle);
    let _idle = notification(&notifier, &seat, &qh, idle, Timer::Idle);

    let mut state = IdleState { tx };
    while !state.tx.is_closed() {
        queue.blocking_dispatch(&mut state)?;
    }
    Ok(())
}

/// Version 2 lets us ignore idle inhibitors: a playing video should not keep us
/// recording an unattended screen.
fn notification(
    notifier: &ExtIdleNotifierV1,
    seat: &WlSeat,
    qh: &QueueHandle<IdleState>,
    timeout: Duration,
    timer: Timer,
) -> ExtIdleNotificationV1 {
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    if notifier.version() >= 2 {
        notifier.get_input_idle_notification(millis, seat, qh, timer)
    } else {
        notifier.get_idle_notification(millis, seat, qh, timer)
    }
}

impl Dispatch<ExtIdleNotificationV1, Timer> for IdleState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        timer: &Timer,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let input = match (event, timer) {
            (ext_idle_notification_v1::Event::Idled, Timer::Settle) => Input::Settled,
            (ext_idle_notification_v1::Event::Idled, Timer::Idle) => Input::Idle,
            (ext_idle_notification_v1::Event::Resumed, _) => Input::Active,
            _ => return,
        };
        let _ = state.tx.send(input);
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for IdleState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(IdleState: ignore WlSeat);
delegate_noop!(IdleState: ExtIdleNotifierV1);
