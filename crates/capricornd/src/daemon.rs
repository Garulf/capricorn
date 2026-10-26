//! Daemon state and the operations on it. Every method here may block on disk or CPU
//! work, so async callers go through `spawn_blocking`.
//!
//! Lock order, outermost first: `unlock_gate`, `ingest_gate`, `store`, `state`.
//! `recorder` and `forgotten` are leaf locks.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
    recorder: Mutex<Option<RecorderState>>,
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
            store: Mutex::new(store),
            unlock_gate: Mutex::new(()),
            ingest_gate: Mutex::new(()),
            recorder: Mutex::new(None),
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
            recorder: *lock(&self.recorder),
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
        *lock(&self.state) = VaultState::Locked {
            vault: Arc::new(vault),
        };
        info!(recovery, "vault created");
        Ok(code.map(|mut code| std::mem::take(&mut *code)))
    }

    /// Grants `conn` a session, unlocking the vault first if needed.
    pub fn unlock(&self, conn: ConnId, credential: Credential<'_>) -> Result<(), DaemonError> {
        let _gate = lock(&self.unlock_gate);
        let vault = lock(&self.state)
            .vault()
            .cloned()
            .ok_or(DaemonError::NotInitialized)?;
        let id = match credential {
            Credential::Password(password) => vault.unlock(password)?,
            Credential::RecoveryCode(code) => vault.unlock_recovery(code)?,
        };
        if let Some(unlocked) = lock(&self.state).unlocked_mut() {
            unlocked.sessions.insert(conn);
            unlocked.extend(self.lock_timeout());
            return Ok(());
        }
        let unlocked = self.open_session(vault, id, conn)?;
        *lock(&self.state) = VaultState::Unlocked(Box::new(unlocked));
        self.pending_index.store(1, Ordering::Relaxed);
        self.catch_up_wanted.notify_one();
        info!("vault unlocked");
        Ok(())
    }

    /// Writes a final snapshot, then forgets the private key and the index.
    pub fn lock_vault(&self, reason: &str) {
        let _gate = lock(&self.unlock_gate);
        let Some(mut unlocked) = lock(&self.state).take_unlocked() else {
            return;
        };
        self.pending_index.store(0, Ordering::Relaxed);
        if let Err(error) = self.save_snapshot(&mut unlocked) {
            warn!(
                error = format!("{error:#}"),
                "could not write index snapshot"
            );
        }
        drop(unlocked);
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
        let id = self.with_session(conn, |unlocked| Ok(unlocked.id.clone()))?;
        let mut vault = Vault::open(&self.data_dir)?;
        vault.unlock(old)?;
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

    pub fn image(&self, conn: ConnId, r: RecordRef, thumb: bool) -> Result<Vec<u8>, DaemonError> {
        let id = self.with_session(conn, |unlocked| Ok(unlocked.id.clone()))?;
        let record = lock(&self.store)
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
        info!(records = refs.len(), "deleted captures");
        Ok(())
    }

    /// Drops captures from the last `minutes` (capped at the seal grace period), which
    /// works while locked because those captures are still in open hours.
    pub fn forget(&self, minutes: u32) -> Result<u32> {
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
            unlocked.index.delete_range(since, Ts::MAX)?;
        }
        info!(minutes, removed, "forgot recent captures");
        Ok(removed)
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
        !self.paused.load(Ordering::Relaxed) && lock(&self.store).is_some()
    }

    pub fn set_recorder(&self, recorder: Option<RecorderState>) {
        *lock(&self.recorder) = recorder;
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

    fn save_snapshot(&self, unlocked: &mut Unlocked) -> Result<()> {
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

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}
