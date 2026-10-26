//! Daemon state and the operations on it. Every method here may block on disk or CPU
//! work, so async callers go through `spawn_blocking`.
//!
//! Lock order, outermost first: `unlock_gate`, `ingest_gate`, `store`, `state`,
//! `snapshot_gate`. `recorders`, `forgotten` and `failures` are leaf locks. Locking the
//! vault never takes `unlock_gate`, so a slow or flooded unlock cannot delay it.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use capricorn_core::Record;
use capricorn_core::config::Config;
use capricorn_core::proto::{CaptureCtl, DeleteScope, Kind, RecordRef, RecorderState, Status, Ts};
use capricorn_index::{Index, Watermark};
use capricorn_vault::segment::{hour_end_ms, hour_start_ms};
use capricorn_vault::{Identity, KdfParams, SegmentStore, Vault, VaultError, plan_retention};
use tokio::sync::{Notify, broadcast};
use tracing::{debug, info, warn};

use crate::index_store::{IndexFiles, IndexKey};
use crate::state::{ConnId, Unlocked, VaultState};

pub const SEGMENTS_DIR: &str = "segments";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MINUTE_MS: i64 = 60_000;
const GIB: u64 = 1 << 30;
const KDF_TARGET: Duration = Duration::from_millis(750);

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("locked")]
    Locked,
    #[error("not initialized")]
    NotInitialized,
    #[error("already initialized")]
    AlreadyInitialized,
    #[error("bad password")]
    BadPassword,
    #[error("password must not be empty")]
    EmptyPassword,
    #[error("still indexing, try again shortly")]
    Indexing,
    #[error("not found")]
    NotFound,
    #[error("busy, try again")]
    Busy,
    #[error("too many attempts, try again later")]
    TooManyAttempts,
    #[error("locked during unlock, try again")]
    LockedDuringUnlock,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<VaultError> for DaemonError {
    fn from(error: VaultError) -> Self {
        match error {
            VaultError::BadPassword => DaemonError::BadPassword,
            VaultError::EmptyPassword => DaemonError::EmptyPassword,
            VaultError::AlreadyExists => DaemonError::AlreadyInitialized,
            VaultError::Locked => DaemonError::Locked,
            VaultError::NotFound(_) => DaemonError::NotFound,
            other => DaemonError::Internal(other.into()),
        }
    }
}

pub enum Credential<'a> {
    Password(&'a str),
    RecoveryCode(&'a str),
}

pub struct Daemon {
    cfg: Config,
    data_dir: PathBuf,
    index_files: IndexFiles,
    state: Mutex<VaultState>,
    store: Mutex<Option<SegmentStore>>,
    unlock_gate: Mutex<()>,
    ingest_gate: Mutex<()>,
    snapshot_gate: Mutex<()>,
    has_store: AtomicBool,
    /// Capture connections in the order they connected, with what each reported.
    recorders: Mutex<Vec<(u64, Option<RecorderState>)>>,
    failures: Mutex<FailedAttempts>,
    /// Bumped by every lock, so an unlock that raced a lock does not install itself.
    lock_epoch: AtomicU64,
    /// Captures stamped inside this window were forgotten while still queued.
    forgotten: Mutex<Option<(Ts, Ts)>>,
    pending_index: AtomicU64,
    paused: AtomicBool,
    epochs: AtomicU64,
    capture_ctl: broadcast::Sender<CaptureCtl>,
    catch_up_wanted: Notify,
}

pub fn now_ms() -> Ts {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as Ts)
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Daemon {
    /// Opens the vault in `cfg.data_dir()` if one exists, creating the directory (mode
    /// 0700) when missing.
    pub fn open(cfg: Config) -> Result<Self> {
        let data_dir = cfg.data_dir();
        create_private_dir(&data_dir)?;
        let (state, store) = if Vault::exists(&data_dir) {
            let vault = Vault::open(&data_dir).context("opening vault")?;
            let store = SegmentStore::open(&data_dir.join(SEGMENTS_DIR), vault.recipient())?;
            let vault = Arc::new(vault);
            (VaultState::Locked { vault }, Some(store))
        } else {
            (VaultState::Uninit, None)
        };
        Ok(Self {
            index_files: IndexFiles::new(&data_dir),
            data_dir,
            cfg,
            state: Mutex::new(state),
            has_store: AtomicBool::new(store.is_some()),
            store: Mutex::new(store),
            unlock_gate: Mutex::new(()),
            ingest_gate: Mutex::new(()),
            snapshot_gate: Mutex::new(()),
            recorders: Mutex::new(Vec::new()),
            failures: Mutex::new(FailedAttempts::default()),
            lock_epoch: AtomicU64::new(0),
            forgotten: Mutex::new(None),
            pending_index: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            epochs: AtomicU64::new(0),
            capture_ctl: broadcast::channel(16).0,
            catch_up_wanted: Notify::new(),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn status(&self) -> Status {
        Status {
            vault: lock(&self.state).describe(Instant::now()),
            recorder: lock(&self.recorders).last().and_then(|(_, state)| *state),
            pending_index: self.pending_index.load(Ordering::Relaxed),
        }
    }

    pub fn init(&self, password: &str, recovery: bool) -> Result<Option<String>, DaemonError> {
        let _gate = lock(&self.unlock_gate);
        if !matches!(*lock(&self.state), VaultState::Uninit) {
            return Err(DaemonError::AlreadyInitialized);
        }
        let (vault, _identity, code) =
            Vault::create(&self.data_dir, password, self.kdf_params(), recovery)?;
        let store = SegmentStore::open(&self.data_dir.join(SEGMENTS_DIR), vault.recipient())?;
        *lock(&self.store) = Some(store);
        self.has_store.store(true, Ordering::Relaxed);
        *lock(&self.state) = VaultState::Locked {
            vault: Arc::new(vault),
        };
        info!(recovery, "vault created");
        Ok(code.map(|mut code| std::mem::take(&mut *code)))
    }

    /// Grants `conn` a session, unlocking the vault first if needed. One attempt runs at
    /// a time; others are turned away rather than queued, and repeated failures make
    /// every caller wait (see [`FailedAttempts`]).
    pub fn unlock(&self, conn: ConnId, credential: Credential<'_>) -> Result<(), DaemonError> {
        let _gate = match self.unlock_gate.try_lock() {
            Ok(gate) => gate,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(DaemonError::Busy),
        };
        lock(&self.failures).check(Instant::now())?;
        let epoch = self.lock_epoch.load(Ordering::SeqCst);
        let vault = lock(&self.state)
            .vault()
            .cloned()
            .ok_or(DaemonError::NotInitialized)?;
        let id = self.check_credential(&vault, credential)?;
        if let Some(unlocked) = lock(&self.state).unlocked_mut() {
            unlocked.sessions.insert(conn);
            unlocked.extend(self.lock_timeout());
            return Ok(());
        }
        let unlocked = self.open_session(vault, id, conn)?;
        let mut state = lock(&self.state);
        if self.lock_epoch.load(Ordering::SeqCst) != epoch {
            return Err(DaemonError::LockedDuringUnlock);
        }
        *state = VaultState::Unlocked(Box::new(unlocked));
        drop(state);
        self.pending_index.store(1, Ordering::Relaxed);
        self.catch_up_wanted.notify_one();
        info!("vault unlocked");
        Ok(())
    }

    /// Counts a wrong password or recovery code toward the shared backoff.
    fn check_credential(
        &self,
        vault: &Vault,
        credential: Credential<'_>,
    ) -> Result<Identity, DaemonError> {
        let checked = match credential {
            Credential::Password(password) => vault.unlock(password),
            Credential::RecoveryCode(code) => vault.unlock_recovery(code),
        };
        let mut failures = lock(&self.failures);
        match checked {
            Ok(id) => {
                failures.reset();
                Ok(id)
            }
            Err(VaultError::BadPassword) => {
                failures.record(Instant::now());
                Err(DaemonError::BadPassword)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Forgets the private key at once, then writes the final snapshot, which needs only
    /// the index key. Never waits for an unlock in progress; that unlock sees the new
    /// lock epoch and backs out.
    pub fn lock_vault(&self, reason: &str) {
        self.lock_epoch.fetch_add(1, Ordering::SeqCst);
        let mut state = lock(&self.state);
        let Some(unlocked) = state.take_unlocked() else {
            return;
        };
        let _snapshot = lock(&self.snapshot_gate);
        drop(state);
        self.pending_index.store(0, Ordering::Relaxed);
        let Unlocked {
            id,
            mut index,
            watermark,
            index_key,
            ..
        } = *unlocked;
        drop(id);
        if let Err(error) = self.index_files.save(&index_key, &mut index, &watermark) {
            warn!(
                error = format!("{error:#}"),
                "could not write index snapshot"
            );
        }
        info!(reason, "vault locked");
    }

    pub fn lock_if_idle(&self) -> Result<()> {
        let expired = lock(&self.state)
            .unlocked()
            .is_some_and(|unlocked| Instant::now() >= unlocked.deadline);
        if expired {
            self.lock_vault("idle timeout");
        }
        Ok(())
    }

    pub fn change_password(&self, conn: ConnId, old: &str, new: &str) -> Result<(), DaemonError> {
        let _gate = lock(&self.unlock_gate);
        self.with_session(conn, |_| Ok(()))?;
        let mut vault = Vault::open(&self.data_dir)?;
        vault.unlock(old)?;
        let id = self.with_session(conn, |unlocked| Ok(unlocked.id.clone()))?;
        vault.change_password(&id, new, self.kdf_params())?;
        if let Some(unlocked) = lock(&self.state).unlocked_mut() {
            unlocked.vault = Arc::new(vault);
        }
        info!("vault password changed");
        Ok(())
    }

    /// Ends `conn`'s session without locking the vault.
    pub fn end_session(&self, conn: ConnId) {
        if let Some(unlocked) = lock(&self.state).unlocked_mut() {
            unlocked.sessions.remove(&conn);
        }
    }

    /// Refreshes the idle deadline, for session connections only.
    pub fn touch(&self, conn: ConnId) {
        let _ = self.with_session(conn, |_| Ok(()));
    }

    /// Runs `op` on the unlocked state if `conn` holds a session, refreshing the deadline.
    pub fn with_session<T>(
        &self,
        conn: ConnId,
        op: impl FnOnce(&mut Unlocked) -> Result<T, DaemonError>,
    ) -> Result<T, DaemonError> {
        let mut state = lock(&self.state);
        let unlocked = state.session(conn).ok_or(DaemonError::Locked)?;
        unlocked.extend(self.lock_timeout());
        op(unlocked)
    }

    /// Holds the store lock from the session check to the read, so a lock in between
    /// cannot leave this decrypting afterwards.
    pub fn image(&self, conn: ConnId, r: RecordRef, thumb: bool) -> Result<Vec<u8>, DaemonError> {
        let store_guard = lock(&self.store);
        let id = self.with_session(conn, |unlocked| Ok(unlocked.id.clone()))?;
        let record = store_guard
            .as_ref()
            .ok_or(DaemonError::NotFound)?
            .read(r, Some(&id))?;
        let image = if thumb { record.thumb } else { record.image };
        image.ok_or(DaemonError::NotFound)
    }

    pub fn delete(&self, conn: ConnId, scope: DeleteScope) -> Result<(), DaemonError> {
        let mut store_guard = lock(&self.store);
        let store = store_guard.as_mut().ok_or(DaemonError::Locked)?;
        let mut state = lock(&self.state);
        let unlocked = state.session(conn).ok_or(DaemonError::Locked)?;
        unlocked.extend(self.lock_timeout());
        let refs = match scope {
            DeleteScope::One(r) => {
                unlocked.index.delete_ref(r)?;
                vec![r]
            }
            DeleteScope::App(class) => {
                if !unlocked.caught_up {
                    return Err(DaemonError::Indexing);
                }
                unlocked.index.delete_app(&class)?
            }
            DeleteScope::Range { from, to } => delete_range(store, unlocked, from, to)?,
        };
        let id = unlocked.id.clone();
        delete_refs(store, &refs, &id)?;
        self.save_snapshot(unlocked)?;
        info!(records = refs.len(), "deleted captures");
        Ok(())
    }

    /// Drops captures from the last `minutes` (capped at the seal grace period), which
    /// works while locked because those captures are still in open hours. When unlocked,
    /// exactly the removed refs leave the index and the snapshot is rewritten; when
    /// locked, the next catch-up pass drops them.
    pub fn forget(&self, minutes: u32) -> Result<usize> {
        let minutes = minutes.min(self.cfg.forget_grace_mins);
        if minutes == 0 {
            return Ok(0);
        }
        let now = now_ms();
        let since = now - i64::from(minutes) * MINUTE_MS;
        *lock(&self.forgotten) = Some((since, now));
        let mut store_guard = lock(&self.store);
        let Some(store) = store_guard.as_mut() else {
            return Ok(0);
        };
        let removed = store.forget_since(since)?;
        if let Some(unlocked) = lock(&self.state).unlocked_mut() {
            unlocked.index.delete_refs(&removed)?;
            self.save_snapshot(unlocked)?;
        }
        info!(minutes, removed = removed.len(), "forgot recent captures");
        Ok(removed.len())
    }

    /// Seals a processed capture into its hour and, when the index is caught up,
    /// indexes it right away.
    pub fn store_capture(&self, record: Record) -> Result<()> {
        if self.was_forgotten(record.ts) {
            return Ok(());
        }
        let r = {
            let mut store_guard = lock(&self.store);
            let Some(store) = store_guard.as_mut() else {
                debug!("no vault yet; dropping capture");
                return Ok(());
            };
            store.append(&record, now_ms())?
        };
        let mut state = lock(&self.state);
        let Some(unlocked) = state.unlocked_mut() else {
            return Ok(());
        };
        if is_next_for_index(unlocked, r) {
            unlocked.index.add(r, &record)?;
            unlocked.index.commit()?;
            unlocked
                .watermark
                .open_seq
                .insert(r.hour, r.seq.saturating_add(1));
        } else {
            drop(state);
            self.catch_up_wanted.notify_one();
        }
        Ok(())
    }

    pub fn accepts_captures(&self) -> bool {
        !self.paused.load(Ordering::Relaxed) && self.has_store.load(Ordering::Relaxed)
    }

    pub fn capture_connected(&self, capture: u64) {
        lock(&self.recorders).push((capture, None));
    }

    pub fn capture_disconnected(&self, capture: u64) {
        lock(&self.recorders).retain(|(id, _)| *id != capture);
    }

    /// Status shows the newest live capture connection, so a stale one cannot override it.
    pub fn set_recorder(&self, capture: u64, recorder: RecorderState) {
        if let Some(entry) = lock(&self.recorders)
            .iter_mut()
            .find(|(id, _)| *id == capture)
        {
            entry.1 = Some(recorder);
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        let ctl = if paused {
            CaptureCtl::Pause
        } else {
            CaptureCtl::Resume
        };
        let _ = self.capture_ctl.send(ctl);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn subscribe_ctl(&self) -> broadcast::Receiver<CaptureCtl> {
        self.capture_ctl.subscribe()
    }

    pub async fn catch_up_requested(&self) {
        self.catch_up_wanted.notified().await;
    }

    pub fn seal_due(&self) -> Result<()> {
        let sealed = match lock(&self.store).as_mut() {
            Some(store) => store.seal_due(now_ms(), self.cfg.forget_grace_mins)?,
            None => return Ok(()),
        };
        if !sealed.is_empty() && lock(&self.state).unlocked().is_some() {
            self.catch_up_wanted.notify_one();
        }
        Ok(())
    }

    pub fn apply_retention(&self) -> Result<()> {
        let max_bytes = self
            .cfg
            .retention_max_gib
            .map(|gib| gib.saturating_mul(GIB));
        let mut store_guard = lock(&self.store);
        let Some(store) = store_guard.as_mut() else {
            return Ok(());
        };
        let doomed = plan_retention(
            &store.hours()?,
            now_ms(),
            self.cfg.retention_days,
            max_bytes,
        );
        for &hour in &doomed {
            store.remove_hour(hour)?;
        }
        if let Some(unlocked) = lock(&self.state).unlocked_mut() {
            for &hour in &doomed {
                unlocked.forget_hour(hour)?;
            }
        }
        drop(store_guard);
        if !doomed.is_empty() {
            info!(hours = doomed.len(), "retention removed old hours");
        }
        self.apply_clipboard_retention()
    }

    pub fn apply_clipboard_retention(&self) -> Result<()> {
        let Some(days) = self.cfg.clipboard_retention_days else {
            return Ok(());
        };
        let before = now_ms() - i64::from(days) * DAY_MS;
        let mut store_guard = lock(&self.store);
        let Some(store) = store_guard.as_mut() else {
            return Ok(());
        };
        let mut state = lock(&self.state);
        let Some(unlocked) = state.unlocked_mut() else {
            return Ok(());
        };
        let refs = unlocked.index.delete_kind_before(Kind::Clipboard, before)?;
        let id = unlocked.id.clone();
        delete_refs(store, &refs, &id)
    }

    pub fn write_snapshot(&self) -> Result<()> {
        match lock(&self.state).unlocked_mut() {
            Some(unlocked) => self.save_snapshot(unlocked),
            None => Ok(()),
        }
    }

    /// Callers hold `state`, so snapshots land in the order of the states they capture.
    fn save_snapshot(&self, unlocked: &mut Unlocked) -> Result<()> {
        let _snapshot = lock(&self.snapshot_gate);
        self.index_files.save(
            &unlocked.index_key,
            &mut unlocked.index,
            &unlocked.watermark,
        )
    }

    fn open_session(&self, vault: Arc<Vault>, id: Identity, conn: ConnId) -> Result<Unlocked> {
        let (index_key, index, mut watermark) = self.load_index(&id)?;
        let compacted = lock(&self.store)
            .as_mut()
            .context("vault has no segment store")?
            .compact_orphans(&id, now_ms(), self.cfg.forget_grace_mins);
        match compacted {
            Ok(report) => {
                for hour in report.compacted {
                    watermark.sealed_hours.remove(&hour);
                }
                for (path, error) in report.quarantined {
                    warn!(path = %path.display(), %error, "quarantined unreadable segment");
                }
                for (hour, error) in report.failed {
                    warn!(hour, %error, "could not merge orphaned segments for hour");
                }
            }
            Err(error) => warn!(%error, "could not merge orphaned segments"),
        }
        Ok(Unlocked {
            vault,
            id: Arc::new(id),
            index,
            watermark,
            index_key,
            deadline: Instant::now() + self.lock_timeout(),
            sessions: [conn].into(),
            epoch: self.epochs.fetch_add(1, Ordering::Relaxed) + 1,
            caught_up: false,
        })
    }

    /// An unreadable key or snapshot is replaced by an empty index rebuilt from the
    /// segments, which hold everything the index did.
    fn load_index(&self, id: &Identity) -> Result<(IndexKey, Index, Watermark)> {
        let _snapshot = lock(&self.snapshot_gate);
        let loaded = self.index_files.load_or_create_key(id).and_then(|key| {
            let snapshot = self.index_files.load(&key)?;
            Ok((key, snapshot))
        });
        match loaded {
            Ok((key, Some((index, watermark)))) => Ok((key, index, watermark)),
            Ok((key, None)) => Ok((key, Index::new()?, Watermark::default())),
            Err(error) => {
                warn!(
                    error = format!("{error:#}"),
                    "index unreadable; rebuilding it"
                );
                let key = self.index_files.create_key(id)?;
                Ok((key, Index::new()?, Watermark::default()))
            }
        }
    }

    fn was_forgotten(&self, ts: Ts) -> bool {
        lock(&self.forgotten).is_some_and(|(since, at)| (since..=at).contains(&ts))
    }

    fn lock_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.lock_timeout_secs)
    }

    fn kdf_params(&self) -> KdfParams {
        match self.cfg.kdf {
            Some(kdf) => KdfParams {
                m_kib: kdf.memory_kib,
                t: kdf.iterations,
                p: kdf.parallelism,
            },
            None => KdfParams::tune(KDF_TARGET),
        }
    }

    pub(crate) fn pending_index(&self) -> &AtomicU64 {
        &self.pending_index
    }

    pub(crate) fn ingest_gate(&self) -> MutexGuard<'_, ()> {
        lock(&self.ingest_gate)
    }

    pub(crate) fn store(&self) -> MutexGuard<'_, Option<SegmentStore>> {
        lock(&self.store)
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, VaultState> {
        lock(&self.state)
    }

    pub(crate) fn snapshot_after_ingest(&self, unlocked: &mut Unlocked) -> Result<()> {
        self.save_snapshot(unlocked)
    }
}

/// True when every earlier record of the hour is already indexed, so adding `r` keeps
/// the watermark exact.
fn is_next_for_index(unlocked: &Unlocked, r: RecordRef) -> bool {
    let watermark = &unlocked.watermark;
    unlocked.caught_up
        && !watermark.sealed_hours.contains(&r.hour)
        && watermark.open_seq.get(&r.hour).copied().unwrap_or(0) == r.seq
}

/// Whole hours inside the range are removed outright; the boundary hours are scanned
/// so captures that were never indexed go too.
fn delete_range(
    store: &mut SegmentStore,
    unlocked: &mut Unlocked,
    from: Ts,
    to: Ts,
) -> Result<Vec<RecordRef>> {
    if from > to {
        return Ok(Vec::new());
    }
    let mut refs = unlocked.index.delete_range(from, to)?;
    for info in store.hours()? {
        let (start, end) = (hour_start_ms(info.hour), hour_end_ms(info.hour));
        if start >= from && end - 1 <= to {
            store.remove_hour(info.hour)?;
            unlocked.forget_hour(info.hour)?;
        } else if end > from && start <= to {
            let in_range = store
                .records_in(info.hour, &unlocked.id)?
                .into_iter()
                .filter(|(_, record)| (from..=to).contains(&record.ts))
                .map(|(r, _)| r);
            refs.extend(in_range);
        }
    }
    Ok(refs)
}

fn delete_refs(store: &mut SegmentStore, refs: &[RecordRef], id: &Identity) -> Result<()> {
    let mut by_hour: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for r in refs {
        by_hour.entry(r.hour).or_default().push(r.seq);
    }
    for (hour, seqs) in by_hour {
        store.delete_records(hour, &seqs, id)?;
    }
    Ok(())
}

/// Shared by every connection, so opening more connections buys no more guesses.
#[derive(Default)]
struct FailedAttempts {
    consecutive: u32,
    last: Option<Instant>,
}

impl FailedAttempts {
    /// Failures allowed before attempts are spaced out.
    const FREE: u32 = 3;
    const MAX_WAIT: Duration = Duration::from_secs(60);

    fn record(&mut self, at: Instant) {
        self.consecutive = self.consecutive.saturating_add(1);
        self.last = Some(at);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    /// After `FREE` failures in a row the next attempt waits 1 s from the last failure,
    /// doubling with each further failure up to a minute.
    fn wait(&self) -> Duration {
        let Some(extra) = self.consecutive.checked_sub(Self::FREE + 1) else {
            return Duration::ZERO;
        };
        Duration::from_secs(1u64 << extra.min(6)).min(Self::MAX_WAIT)
    }

    fn check(&self, now: Instant) -> Result<(), DaemonError> {
        match self.last {
            Some(last) if now < last + self.wait() => Err(DaemonError::TooManyAttempts),
            _ => Ok(()),
        }
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_attempts_back_off_after_the_free_ones() {
        let start = Instant::now();
        let mut failures = FailedAttempts::default();
        for _ in 0..FailedAttempts::FREE {
            assert!(failures.check(start).is_ok());
            failures.record(start);
        }
        assert!(failures.check(start).is_ok());
        failures.record(start);
        assert!(failures.check(start + Duration::from_millis(500)).is_err());
        assert!(failures.check(start + Duration::from_secs(1)).is_ok());
        failures.record(start);
        assert!(failures.check(start + Duration::from_secs(1)).is_err());
        assert!(failures.check(start + Duration::from_secs(2)).is_ok());
        for _ in 0..20 {
            failures.record(start);
        }
        assert_eq!(failures.wait(), FailedAttempts::MAX_WAIT);
        failures.reset();
        assert!(failures.check(start).is_ok());
    }
}
