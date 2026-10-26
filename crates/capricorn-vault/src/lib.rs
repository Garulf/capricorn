//! capricorn-vault
pub mod keyfile;
pub mod segment;
pub mod stream;

pub use keyfile::{
    Identity, KdfParams, KeyFile, Recipient, Result, Slot, SlotKind, Vault, VaultError, open_small,
    seal_small,
};
pub use segment::{SegmentInfo, SegmentStore};
