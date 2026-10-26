//! capricorn-vault
pub mod keyfile;
pub mod retention;
pub mod segment;
pub mod stream;

pub use keyfile::{
    Identity, KdfParams, KeyFile, Recipient, Result, Slot, SlotKind, Vault, VaultError, open_small,
    seal_small,
};
pub use retention::plan_retention;
pub use segment::{CompactReport, SegmentInfo, SegmentStore};
