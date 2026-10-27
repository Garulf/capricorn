use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use rustix::pipe::{PipeFlags, pipe_with};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use crate::wl;

pub const PASSWORD_HINT_MIME: &str = "x-kde-passwordManagerHint";
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_HINT_BYTES: usize = 64;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(1);

/// A text selection someone copied. No `Debug`: the text may be sensitive.
pub struct ClipText {
    pub text: String,
    /// When the new selection was announced, before its text was read.
    pub seen_at: Instant,
}

/// Password managers mark their copies with a hint MIME whose payload is `secret`. A hint we
/// cannot read is treated as a secret too.
pub fn is_password_manager_offer(
    mimes: &[String],
    read_hint: impl Fn(&str) -> Option<Vec<u8>>,
) -> bool {
    if !mimes.iter().any(|m| m == PASSWORD_HINT_MIME) {
        return false;
    }
    read_hint(PASSWORD_HINT_MIME).is_none_or(|hint| hint.trim_ascii() == b"secret")
}

/// Returns the offered spelling, which is what `receive` must ask for.
fn pick_text_mime(mimes: &[String]) -> Option<&str> {
    TEXT_MIMES.iter().find_map(|wanted| {
        mimes
            .iter()
            .find(|offered| offered.eq_ignore_ascii_case(wanted))
            .map(String::as_str)
    })
}

/// Watches the regular selection via ext-data-control-v1, falling back to
/// wlr-data-control-unstable-v1. Whatever was on the clipboard at startup is skipped.
pub fn watch(conn: Connection, tx: UnboundedSender<ClipText>) -> anyhow::Result<()> {
    let (globals, mut queue) = registry_queue_init::<ClipState>(&conn)?;
    let qh = queue.handle();
    let seat: WlSeat = wl::bind_all(&globals, &qh, 1)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no wl_seat"))?;
    if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
        manager.get_data_device(&seat, &qh, ());
    } else {
        let manager: ZwlrDataControlManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .context("neither ext-data-control-v1 nor wlr-data-control is available")?;
        manager.get_data_device(&seat, &qh, ());
    }

    let mut state = ClipState::default();
    queue.roundtrip(&mut state)?;
    state.discard_selection();

    let mut last_hash = None;
    while !tx.is_closed() {
        queue.blocking_dispatch(&mut state)?;
        if state.finished {
            bail!("data-control device was finished by the compositor");
        }
        let Some(offer) = state.selection.take() else {
            continue;
        };
        let seen_at = Instant::now();
        let mimes = state.mimes.remove(&offer.id()).unwrap_or_default();
        let text = read_text(&conn, &offer, &mimes);
        offer.destroy();
        if let Some(text) = text {
            let hash = text_hash(&text);
            if last_hash != Some(hash) {
                last_hash = Some(hash);
                let _ = tx.send(ClipText { text, seen_at });
            }
        }
    }
    Ok(())
}

fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn read_text(conn: &Connection, offer: &Offer, mimes: &[String]) -> Option<String> {
    let hint = |mime: &str| receive(conn, offer, mime, MAX_HINT_BYTES);
    if is_password_manager_offer(mimes, hint) {
        return None;
    }
    let mime = pick_text_mime(mimes)?;
    let bytes = receive(conn, offer, mime, MAX_TEXT_BYTES)?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

fn receive(conn: &Connection, offer: &Offer, mime: &str, limit: usize) -> Option<Vec<u8>> {
    let (reader, writer) = pipe_with(PipeFlags::CLOEXEC).ok()?;
    offer.receive(mime, writer);
    conn.flush().ok()?;
    read_limited(reader, limit, SOURCE_TIMEOUT)
}

/// Reads to EOF, giving up when the source is slow or sends more than `limit` bytes.
fn read_limited(fd: OwnedFd, limit: usize, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !wl::wait_readable(fd.as_fd(), remaining).ok()? {
            return None;
        }
        match rustix::io::read(&fd, &mut chunk) {
            Ok(0) => return Some(data),
            Ok(n) if data.len() + n > limit => return None,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
            Err(_) => return None,
        }
    }
}

enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Offer::Ext(o) => o.id(),
            Offer::Wlr(o) => o.id(),
        }
    }

    /// Takes ownership of the write end so it is closed here once handed to the compositor.
    fn receive(&self, mime: &str, writer: OwnedFd) {
        match self {
            Offer::Ext(o) => o.receive(mime.to_owned(), writer.as_fd()),
            Offer::Wlr(o) => o.receive(mime.to_owned(), writer.as_fd()),
        }
    }

    fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

#[derive(Default)]
struct ClipState {
    mimes: HashMap<ObjectId, Vec<String>>,
    selection: Option<Offer>,
    finished: bool,
}

impl ClipState {
    fn add_mime(&mut self, offer: ObjectId, mime: String) {
        self.mimes.entry(offer).or_default().push(mime);
    }

    fn set_selection(&mut self, offer: Option<Offer>) {
        self.discard_selection();
        self.selection = offer;
    }

    fn discard_selection(&mut self) {
        if let Some(old) = self.selection.take() {
            self.forget(&old);
        }
    }

    fn forget(&mut self, offer: &Offer) {
        self.mimes.remove(&offer.id());
        offer.destroy();
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::Selection { id } => state.set_selection(id.map(Offer::Ext)),
            Event::PrimarySelection { id: Some(offer) } => state.forget(&Offer::Ext(offer)),
            Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(ClipState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.add_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::Selection { id } => state.set_selection(id.map(Offer::Wlr)),
            Event::PrimarySelection { id: Some(offer) } => state.forget(&Offer::Wlr(offer)),
            Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(ClipState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.add_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for ClipState {
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

delegate_noop!(ClipState: ignore WlSeat);
delegate_noop!(ClipState: ExtDataControlManagerV1);
delegate_noop!(ClipState: ZwlrDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn password_hint_detected() {
        let offered = mimes(&["text/plain", PASSWORD_HINT_MIME]);
        let asked = Cell::new(None);
        let secret = is_password_manager_offer(&offered, |mime| {
            asked.set(Some(mime.to_owned()));
            Some(b"secret".to_vec())
        });
        assert!(secret);
        assert_eq!(asked.take().as_deref(), Some(PASSWORD_HINT_MIME));
    }

    #[test]
    fn password_hint_tolerates_trailing_whitespace() {
        let offered = mimes(&[PASSWORD_HINT_MIME]);
        assert!(is_password_manager_offer(&offered, |_| Some(
            b"secret\n".to_vec()
        )));
    }

    #[test]
    fn other_hint_values_are_not_secret() {
        let offered = mimes(&["text/plain", PASSWORD_HINT_MIME]);
        assert!(!is_password_manager_offer(&offered, |_| Some(
            b"public".to_vec()
        )));
    }

    #[test]
    fn unreadable_hint_counts_as_secret() {
        let offered = mimes(&["text/plain", PASSWORD_HINT_MIME]);
        assert!(is_password_manager_offer(&offered, |_| None));
    }

    #[test]
    fn no_hint_is_not_secret_and_hint_is_not_read() {
        let offered = mimes(&["text/plain", "UTF8_STRING"]);
        let secret = is_password_manager_offer(&offered, |mime| panic!("read {mime}"));
        assert!(!secret);
    }

    #[test]
    fn text_mime_preference() {
        assert_eq!(
            pick_text_mime(&mimes(&["TEXT", "text/plain", "text/plain;charset=utf-8"])),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(
            pick_text_mime(&mimes(&["STRING", "UTF8_STRING"])),
            Some("UTF8_STRING")
        );
        assert_eq!(pick_text_mime(&mimes(&["image/png", "text/html"])), None);
    }
}
