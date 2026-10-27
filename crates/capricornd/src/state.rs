//! The vault's lock state and the per-connection sessions of an unlocked vault.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use capricorn_core::proto;
use capricorn_index::{Index, Watermark};
use capricorn_vault::{Identity, Vault};

use crate::index_store::IndexKey;

/// Identifies one client socket connection for the lifetime of the daemon.
pub type ConnId = u64;

pub enum VaultState {
    Uninit,
    Locked { vault: Arc<Vault> },
    Unlocked(Box<Unlocked>),
}

/// Everything that exists only while the vault is unlocked. Dropping it forgets the
/// private key and the decrypted index.
pub struct Unlocked {
    pub vault: Arc<Vault>,
    pub id: Arc<Identity>,
    pub index: Index,
    pub watermark: Watermark,
    pub index_key: IndexKey,
    pub deadline: Instant,
    /// Connections that proved the password; only they may read or delete.
    pub sessions: HashSet<ConnId>,
    /// Distinguishes this unlock from later ones, so background work started for it
    /// stops once the vault has been locked in between.
    pub epoch: u64,
    /// False until the first ingest pass after unlock finished.
    pub caught_up: bool,
}

impl VaultState {
    pub fn vault(&self) -> Option<&Arc<Vault>> {
        match self {
            VaultState::Uninit => None,
            VaultState::Locked { vault } => Some(vault),
            VaultState::Unlocked(unlocked) => Some(&unlocked.vault),
        }
    }

    pub fn unlocked(&self) -> Option<&Unlocked> {
        match self {
            VaultState::Unlocked(unlocked) => Some(unlocked),
            _ => None,
        }
    }

    pub fn unlocked_mut(&mut self) -> Option<&mut Unlocked> {
        match self {
            VaultState::Unlocked(unlocked) => Some(unlocked),
            _ => None,
        }
    }

    /// The unlocked state, but only for a connection that performed the unlock.
    pub fn session(&mut self, conn: ConnId) -> Option<&mut Unlocked> {
        self.unlocked_mut()
            .filter(|unlocked| unlocked.sessions.contains(&conn))
    }

    pub fn has_session(&self, conn: ConnId) -> bool {
        self.unlocked()
            .is_some_and(|unlocked| unlocked.sessions.contains(&conn))
    }

    /// Replaces an unlocked state with a locked one and hands back what was unlocked.
    pub fn take_unlocked(&mut self) -> Option<Box<Unlocked>> {
        let vault = self.unlocked()?.vault.clone();
        match std::mem::replace(self, VaultState::Locked { vault }) {
            VaultState::Unlocked(unlocked) => Some(unlocked),
            _ => unreachable!("checked above that the state is unlocked"),
        }
    }

    pub fn describe(&self, now: Instant) -> proto::VaultState {
        match self {
            VaultState::Uninit => proto::VaultState::Uninitialized,
            VaultState::Locked { .. } => proto::VaultState::Locked,
            VaultState::Unlocked(unlocked) => proto::VaultState::Unlocked {
                locks_in_secs: unlocked.deadline.saturating_duration_since(now).as_secs(),
            },
        }
    }
}

impl Unlocked {
    pub fn extend(&mut self, timeout: Duration) {
        self.deadline = Instant::now() + timeout;
    }

    /// Drops every trace of `hour` from the index and the watermark.
    pub fn forget_hour(&mut self, hour: u32) -> anyhow::Result<()> {
        self.index.delete_hour(hour)?;
        self.watermark.sealed_hours.remove(&hour);
        self.watermark.open_seq.remove(&hour);
        Ok(())
    }
}
