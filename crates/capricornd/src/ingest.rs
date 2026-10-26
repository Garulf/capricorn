//! Bringing the index up to date with the segments after an unlock, after hours are
//! sealed, and whenever a capture could not be indexed in order.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use capricorn_core::Record;
use capricorn_core::proto::RecordRef;
use capricorn_vault::{Identity, SegmentInfo};
use tracing::{info, warn};

use crate::daemon::Daemon;
use crate::state::{Unlocked, VaultState};

impl Daemon {
    /// One pass over every hour the index may be missing. Hours sealed and recorded in
    /// the watermark are skipped; any other hour is decrypted and its index entries are
    /// rebuilt from the records, so ones the segments no longer hold (forgotten while
    /// locked, for example) go. Stops early once the vault is locked.
    pub fn catch_up(&self) -> Result<()> {
        let _gate = self.ingest_gate();
        let result = self.catch_up_pass();
        if result.is_err() {
            self.pending_index().store(0, Ordering::Relaxed);
        }
        result
    }

    fn catch_up_pass(&self) -> Result<()> {
        let Some((epoch, id)) = self
            .state()
            .unlocked()
            .map(|unlocked| (unlocked.epoch, unlocked.id.clone()))
        else {
            return Ok(());
        };
        let Some(todo) = self.hours_to_ingest(epoch)? else {
            return Ok(());
        };
        self.pending_index()
            .store(todo.len() as u64, Ordering::Relaxed);
        let mut reconciled = 0;
        for info in todo {
            match self.ingest_hour(epoch, &id, info) {
                Ok(Some(())) => reconciled += 1,
                Ok(None) => return Ok(()),
                Err(error) => warn!(
                    hour = info.hour,
                    error = format!("{error:#}"),
                    "skipping unreadable hour"
                ),
            }
            self.pending_index().fetch_sub(1, Ordering::Relaxed);
        }
        self.finish_pass(epoch, reconciled)?;
        self.apply_clipboard_retention()
    }

    /// Also drops index entries for hours whose files are gone (retention while locked).
    fn hours_to_ingest(&self, epoch: u64) -> Result<Option<Vec<SegmentInfo>>> {
        let store_guard = self.store();
        let on_disk = store_guard
            .as_ref()
            .context("vault has no segment store")?
            .hours()?;
        let mut state = self.state();
        let Some(unlocked) = current(&mut state, epoch) else {
            return Ok(None);
        };
        prune_missing_hours(unlocked, &on_disk)?;
        let sealed = &unlocked.watermark.sealed_hours;
        Ok(Some(
            on_disk
                .into_iter()
                .filter(|info| !(info.sealed && sealed.contains(&info.hour)))
                .collect(),
        ))
    }

    fn ingest_hour(&self, epoch: u64, id: &Arc<Identity>, info: SegmentInfo) -> Result<Option<()>> {
        let store_guard = self.store();
        let store = store_guard.as_ref().context("vault has no segment store")?;
        if current(&mut self.state(), epoch).is_none() {
            return Ok(None);
        }
        let records = store.records_in(info.hour, id)?;
        let next_seq = store.next_seq(info.hour);
        let mut state = self.state();
        let Some(unlocked) = current(&mut state, epoch) else {
            return Ok(None);
        };
        reconcile_hour(unlocked, info, &records, next_seq).map(Some)
    }

    fn finish_pass(&self, epoch: u64, reconciled: usize) -> Result<()> {
        let mut state = self.state();
        let Some(unlocked) = current(&mut state, epoch) else {
            return Ok(());
        };
        let first_pass = !unlocked.caught_up;
        unlocked.caught_up = true;
        self.pending_index().store(0, Ordering::Relaxed);
        if first_pass || reconciled > 0 {
            info!(hours = reconciled, "index caught up");
            self.snapshot_after_ingest(unlocked)?;
        }
        Ok(())
    }
}

fn current(state: &mut VaultState, epoch: u64) -> Option<&mut Unlocked> {
    state
        .unlocked_mut()
        .filter(|unlocked| unlocked.epoch == epoch)
}

fn prune_missing_hours(unlocked: &mut Unlocked, on_disk: &[SegmentInfo]) -> Result<()> {
    let present: BTreeSet<u32> = on_disk.iter().map(|info| info.hour).collect();
    for hour in unlocked.index.indexed_hours() {
        if !present.contains(&hour) {
            unlocked.forget_hour(hour)?;
        }
    }
    let watermark = &mut unlocked.watermark;
    watermark.sealed_hours.retain(|hour| present.contains(hour));
    watermark.open_seq.retain(|hour, _| present.contains(hour));
    Ok(())
}

/// Replaces every index entry of the hour with the records the segments hold, so the
/// index matches them exactly even if a ref was forgotten and its seq reused by an older
/// build. `next_seq` is the store's counter for a live hour, which can be past the last
/// record when the newest ones were forgotten.
fn reconcile_hour(
    unlocked: &mut Unlocked,
    info: SegmentInfo,
    records: &[(RecordRef, Record)],
    next_seq: Option<u32>,
) -> Result<()> {
    unlocked.index.delete_hour(info.hour)?;
    for (r, record) in records {
        unlocked.index.add(*r, record)?;
    }
    unlocked.index.commit()?;
    let watermark = &mut unlocked.watermark;
    if info.sealed {
        watermark.sealed_hours.insert(info.hour);
        watermark.open_seq.remove(&info.hour);
    } else {
        let after_last = records.last().map_or(0, |(r, _)| r.seq.saturating_add(1));
        let next = next_seq.map_or(after_last, |next| next.max(after_last));
        watermark.open_seq.insert(info.hour, next);
    }
    Ok(())
}
