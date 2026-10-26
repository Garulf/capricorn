//! capricorn-vault
pub mod keyfile;

pub use keyfile::{
    Identity, KdfParams, KeyFile, Recipient, Result, Slot, SlotKind, Vault, VaultError, open_small,
    seal_small,
};
