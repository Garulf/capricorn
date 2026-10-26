use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::secret::Secret;

/// Unix milliseconds.
pub type Ts = i64;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct WindowContext {
    pub class: String,
    pub title: String,
    pub workspace: String,
    pub output: String,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Excluded regions are already blacked out in `rgba`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ts: Ts,
    pub output: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub window: WindowContext,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("ts", &self.ts)
            .field("output", &self.output)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba_len", &self.rgba.len())
            .field("window", &self.window)
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ClipboardEntry {
    pub ts: Ts,
    pub text: String,
    pub source: WindowContext,
}

impl fmt::Debug for ClipboardEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardEntry")
            .field("ts", &self.ts)
            .field("text_len", &self.text.len())
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderState {
    Recording,
    Paused,
    Idle,
    Locked,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CaptureMsg {
    Frame(Frame),
    Clipboard(ClipboardEntry),
    Forget { minutes: u32 },
    Recorder(RecorderState),
}

/// Sent by the daemon to the capture client on its single connection.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureCtl {
    Pause,
    Resume,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Kind {
    #[default]
    Screen,
    Clipboard,
}

/// `hour` is unix seconds divided by 3600.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordRef {
    pub hour: u32,
    pub seq: u32,
}

impl RecordRef {
    pub fn to_u64(self) -> u64 {
        (u64::from(self.hour) << 32) | u64::from(self.seq)
    }

    pub fn from_u64(value: u64) -> Self {
        Self {
            hour: (value >> 32) as u32,
            seq: value as u32,
        }
    }
}

impl fmt::Display for RecordRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.hour, self.seq)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected record reference like `hour:seq`")]
pub struct ParseRecordRefError;

impl FromStr for RecordRef {
    type Err = ParseRecordRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hour, seq) = s.split_once(':').ok_or(ParseRecordRefError)?;
        Ok(Self {
            hour: hour.parse().map_err(|_| ParseRecordRefError)?,
            seq: seq.parse().map_err(|_| ParseRecordRefError)?,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub kind: Option<Kind>,
    pub app: Option<String>,
    pub workspace: Option<String>,
    pub from: Option<Ts>,
    pub to: Option<Ts>,
    pub limit: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub r: RecordRef,
    pub ts: Ts,
    pub kind: Kind,
    pub app: String,
    pub title: String,
    pub workspace: String,
    pub context: Option<String>,
    pub snippet: String,
    /// Byte ranges into `snippet`.
    pub highlights: Vec<(u32, u32)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TimelineEntry {
    pub r: RecordRef,
    pub ts: Ts,
    pub kind: Kind,
    pub app: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RecordDetail {
    pub hit: Hit,
    pub text: String,
    pub cwd: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum DeleteScope {
    One(RecordRef),
    Range { from: Ts, to: Ts },
    App(String),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultState {
    Uninitialized,
    Locked,
    Unlocked { locks_in_secs: u64 },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub vault: VaultState,
    pub recorder: Option<RecorderState>,
    pub pending_index: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ClientReq {
    Status,
    Init { password: Secret, recovery: bool },
    Unlock { password: Secret },
    UnlockRecovery { code: Secret },
    Lock,
    ChangePassword { old: Secret, new: Secret },
    Search(SearchQuery),
    Timeline { from: Ts, to: Ts },
    Detail(RecordRef),
    Image { r: RecordRef, thumb: bool },
    Delete(DeleteScope),
    Forget { minutes: u32 },
    Pause,
    Resume,
    Touch,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ClientResp {
    Status(Status),
    Ok,
    Err(String),
    RecoveryCode(Secret),
    Hits(Vec<Hit>),
    Timeline(Vec<TimelineEntry>),
    Detail(RecordDetail),
    Image(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_ref_u64_roundtrip_and_order() {
        let a = RecordRef {
            hour: 5,
            seq: u32::MAX,
        };
        let b = RecordRef { hour: 6, seq: 0 };
        assert_eq!(RecordRef::from_u64(a.to_u64()), a);
        assert_eq!(a.to_u64(), (5u64 << 32) | u64::from(u32::MAX));
        assert!(a < b);
        assert!(a.to_u64() < b.to_u64());
    }

    #[test]
    fn record_ref_display_parse() {
        let r = RecordRef {
            hour: 491_000,
            seq: 7,
        };
        assert_eq!(r.to_string(), "491000:7");
        assert_eq!("491000:7".parse::<RecordRef>().unwrap(), r);
        for bad in ["", "1", "a:b", "1:", ":2", "1:2:3", "-1:2"] {
            assert!(bad.parse::<RecordRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn password_requests_do_not_debug_print_secrets() {
        let req = ClientReq::ChangePassword {
            old: Secret::new("old-pass".into()),
            new: Secret::new("new-pass".into()),
        };
        let shown = format!("{req:?}");
        assert!(!shown.contains("old-pass") && !shown.contains("new-pass"));
        let resp = ClientResp::RecoveryCode(Secret::new("ABCD-EFGH".into()));
        assert!(!format!("{resp:?}").contains("ABCD"));
    }

    #[test]
    fn frame_debug_omits_pixels() {
        let frame = Frame {
            ts: 1,
            output: "DP-1".into(),
            width: 2,
            height: 1,
            rgba: vec![0xAB; 8],
            window: WindowContext {
                class: "c".into(),
                title: "t".into(),
                workspace: "1".into(),
                output: "DP-1".into(),
                pid: None,
                cwd: None,
            },
        };
        assert!(format!("{frame:?}").contains("rgba_len: 8"));
    }
}
