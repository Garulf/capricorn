use std::io;
use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::globals::GlobalList;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

pub fn connect() -> anyhow::Result<Connection> {
    Connection::connect_to_env().context("connecting to the Wayland compositor")
}

/// Binds every advertised instance of a multi-instance global such as `wl_output`.
pub fn bind_all<I, S>(globals: &GlobalList, qh: &QueueHandle<S>, max_version: u32) -> Vec<I>
where
    I: Proxy + 'static,
    S: Dispatch<I, ()> + 'static,
{
    let interface = I::interface().name;
    let instances: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == interface)
            .map(|g| (g.name, g.version.min(max_version)))
            .collect()
    });
    instances
        .into_iter()
        .map(|(name, version)| globals.registry().bind::<I, (), S>(name, version, qh, ()))
        .collect()
}

/// Dispatches events into `state` until `done` holds, failing after `timeout` so a
/// compositor that never answers (for example a pending permission prompt) cannot wedge us.
pub fn dispatch_until<S>(
    queue: &mut EventQueue<S>,
    state: &mut S,
    timeout: Duration,
    mut done: impl FnMut(&S) -> bool,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        queue.dispatch_pending(state)?;
        if done(state) {
            return Ok(());
        }
        queue.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for the compositor");
        }
        if wait_readable(guard.connection_fd(), remaining)? {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> io::Result<bool> {
    let timeout = Timespec::try_from(timeout).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    match poll(&mut fds, Some(&timeout)) {
        Ok(ready) => Ok(ready > 0),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
