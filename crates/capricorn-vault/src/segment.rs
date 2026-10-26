//! Hourly encrypted segments: append while locked, seal per hour, read back with the
//! private key.

mod format;
mod open_file;
mod sealed_file;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use capricorn_core::Record;
use capricorn_core::proto::{RecordRef, Ts};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::keyfile::{Identity, Recipient, Result, VaultError};
use format::{
    FileKind, HourKey, SegFile, corrupt, list_files, open_path, quarantine, quarantined_files,
    remove_durably, remove_stale_temps, sealed_path,
};
pub use format::{HOUR_MS, hour_end_ms, hour_of, hour_start_ms};
use open_file::Frame;
use sealed_file::{SealedFile, SealedTable, SealedWriter};

/// Disk usage of one hour bucket. `sealed` is false while any `.open` file (live or
/// orphan) exists for the hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentInfo {
    pub hour: u32,
    pub sealed: bool,
    pub bytes: u64,
}

/// Outcome of [`SegmentStore::compact_orphans`]. Failures are per hour, so one bad file
/// never blocks the other hours.
#[derive(Debug, Default)]
pub struct CompactReport {
    /// Hours whose orphans were merged into a live segment or a `.seg`.
    pub compacted: Vec<u32>,
    /// Orphans that could not be read, renamed to `<name>.bad` (still only ciphertext).
    pub quarantined: Vec<(PathBuf, VaultError)>,
    /// Hours that failed for a reason no single orphan explains, such as an I/O error or
    /// a corrupt `.seg`. Their orphans are left in place for the next call.
    pub failed: Vec<(u32, VaultError)>,
}

/// Hourly segment files under one directory.
///
/// Records are appended to `<hour>.open` under a per-hour key that lives only in memory
/// (sealed to the vault recipient in the file header), so appending and reading the open
/// hours works while the vault is locked. [`SegmentStore::seal_due`] turns an hour into
/// `<hour>.seg`, readable only with the [`Identity`]. Several hours can be open at once
/// because sealing waits for a grace period after the hour ends.
///
/// An `.open` file whose key is not in memory (the daemon restarted) is an orphan. Its
/// records stay on disk untouched until [`SegmentStore::compact_orphans`] merges them
/// using the identity. Because an orphan's timestamps cannot be read while locked,
/// [`SegmentStore::forget_since`] deletes every orphan whose hour could overlap the
/// window. This is deliberate: forgetting too much is the safe direction, keeping
/// something the user asked to forget is not.
///
/// `forget_since` reaches open hours only, so the caller must keep `grace_mins` at least
/// as long as its longest forget window.
///
/// Until an hour is sealed its `.open` file reveals the size of each record's ciphertext
/// and the record count. File mtimes and atimes are reset to the hour start after every
/// write; ctime and the directory's times still change on each write.
///
/// Mutation takes `&mut self`; the store is `Send + Sync`. A process must not open two
/// stores on the same directory.
pub struct SegmentStore {
    dir: PathBuf,
    recipient: Recipient,
    live: BTreeMap<u32, LiveSegment>,
    tables: Mutex<HashMap<u32, Arc<SealedTable>>>,
}

impl SegmentStore {
    /// Opens or creates `dir` (mode 0700). Every existing `.open` file becomes an orphan.
    pub fn open(dir: &Path, recipient: Recipient) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        remove_stale_temps(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            recipient,
            live: BTreeMap::new(),
            tables: Mutex::new(HashMap::new()),
        })
    }

    /// Appends to the newest open hour, or opens the hour of `now` if that is later. The
    /// bucket depends on `now` only, so a bogus `rec.ts` cannot pin captures to an hour
    /// that never seals; `rec.ts` is stored as is. A clock that went backwards keeps
    /// writing to the current hour, and an hour that already has a `.seg` is never reopened.
    pub fn append(&mut self, rec: &Record, now: Ts) -> Result<RecordRef> {
        let hour = self.target_hour(now)?;
        let plaintext = encode(rec)?;
        let seq = self
            .live_segment(hour)?
            .append(&plaintext, rec.ts.max(now))?;
        Ok(RecordRef { hour, seq })
    }

    /// Removes records with `max(ts, append time) >= since` from the open hours and
    /// deletes orphans that could hold such records. Returns how many records were removed.
    pub fn forget_since(&mut self, since: Ts) -> Result<u32> {
        let mut removed = 0;
        for segment in self.live.values_mut() {
            removed += segment.retain(|frame| frame.ts < since)?;
        }
        for orphan in self.orphans()? {
            if hour_end_ms(orphan.hour) > since {
                removed += open_file::scan(&orphan.path).map_or(0, |scan| scan.frames.len() as u32);
                remove_durably(&orphan.path)?;
            }
        }
        Ok(removed)
    }

    /// Seals every open hour with `now >= hour_end + grace_mins`. Returns the hours that
    /// now have a `.seg` written from memory.
    pub fn seal_due(&mut self, now: Ts, grace_mins: u32) -> Result<Vec<u32>> {
        let due: Vec<u32> = self
            .live
            .keys()
            .copied()
            .filter(|&hour| is_due(hour, now, grace_mins))
            .collect();
        let mut sealed = Vec::new();
        for hour in due {
            let segment = self.live.remove(&hour).expect("due hour is live");
            match self.seal_live(&segment) {
                Ok(true) => sealed.push(hour),
                Ok(false) => {}
                Err(error) => {
                    self.live.insert(hour, segment);
                    return Err(error);
                }
            }
        }
        Ok(sealed)
    }

    /// Merges orphan files using the identity. An hour that is due (as in
    /// [`SegmentStore::seal_due`]) or already has a `.seg` is sealed; any other hour gets
    /// a live segment under a fresh in-memory key, so `forget_since` and appends keep
    /// working on it. Seqs are kept, so refs stay valid; a seq that collides with a
    /// different record is renumbered above every existing seq of the hour.
    pub fn compact_orphans(
        &mut self,
        id: &Identity,
        now: Ts,
        grace_mins: u32,
    ) -> Result<CompactReport> {
        let mut by_hour: BTreeMap<u32, Vec<SegFile>> = BTreeMap::new();
        for orphan in self.orphans()? {
            by_hour.entry(orphan.hour).or_default().push(orphan);
        }
        let mut report = CompactReport::default();
        for (hour, orphans) in by_hour {
            let seal = is_due(hour, now, grace_mins);
            match self.compact_hour(hour, &orphans, id, seal) {
                Ok(()) => report.compacted.push(hour),
                Err(error) => self.recover_hour(hour, orphans, id, seal, error, &mut report),
            }
        }
        Ok(report)
    }

    /// Records of open hours read without `id`; anything else needs it. Costs one file
    /// open and AEAD per record, plus one age unwrap for sealed or orphan records.
    pub fn read(&self, r: RecordRef, id: Option<&Identity>) -> Result<Record> {
        if let Some(segment) = self.live.get(&r.hour)
            && let Some(bytes) = segment.read(r.seq)?
        {
            return decode(&bytes);
        }
        let id = id.ok_or(VaultError::Locked)?;
        if let Some(bytes) = self.read_sealed(r, id)? {
            return decode(&bytes);
        }
        if let Some(bytes) = self.read_orphan(r, id)? {
            return decode(&bytes);
        }
        Err(VaultError::NotFound(r))
    }

    /// Every record of `hour` from all its files, ordered by seq.
    pub fn records_in(&self, hour: u32, id: &Identity) -> Result<Vec<(RecordRef, Record)>> {
        let mut records = BTreeMap::new();
        let mut keep_first = |seq: u32, bytes: Zeroizing<Vec<u8>>| -> Result<()> {
            if let std::collections::btree_map::Entry::Vacant(slot) = records.entry(seq) {
                slot.insert(decode(&bytes)?);
            }
            Ok(())
        };
        if let Some((mut file, table)) = self.open_sealed(hour, id)? {
            for seq in table.seqs() {
                let (offset, len) = table.get(seq).expect("seq comes from table");
                keep_first(seq, file.read_plain(offset, len)?)?;
            }
        }
        if let Some(segment) = self.live.get(&hour) {
            segment.for_each_record(&mut keep_first)?;
        }
        for orphan in self.orphans_of(hour)? {
            let scan = open_file::scan(&orphan.path)?;
            let key = scan.unseal(id, hour)?;
            open_file::for_each_record(
                &orphan.path,
                &key,
                hour,
                &scan.frames,
                true,
                &mut keep_first,
            )?;
        }
        Ok(records
            .into_iter()
            .map(|(seq, record)| (RecordRef { hour, seq }, record))
            .collect())
    }

    /// Removes `seqs` from every file of `hour`, rewriting each affected file. Seqs that
    /// do not exist are ignored. Remaining refs are unchanged.
    pub fn delete_records(&mut self, hour: u32, seqs: &[u32], id: &Identity) -> Result<()> {
        let doomed = |seq: u32| seqs.contains(&seq);
        if let Some(segment) = self.live.get_mut(&hour) {
            segment.retain(|frame| !doomed(frame.frame.seq))?;
        }
        for orphan in self.orphans_of(hour)? {
            let scan = open_file::scan(&orphan.path)?;
            if scan.frames.iter().any(|frame| doomed(frame.seq)) {
                open_file::rewrite_keeping(
                    &orphan.path,
                    hour,
                    scan.header_len,
                    &scan.frames,
                    |frame| !doomed(frame.seq),
                )?;
            }
        }
        if let Some((mut file, table)) = self.open_sealed(hour, id)?
            && table.contains_any(seqs)
        {
            let mut writer = SealedWriter::create(&self.dir, hour, &self.recipient)?;
            for seq in table.seqs().filter(|&seq| !doomed(seq)) {
                let (offset, len) = table.get(seq).expect("seq comes from table");
                writer.add(seq, &file.read_plain(offset, len)?)?;
            }
            writer.commit()?;
            self.forget_table(hour);
        }
        Ok(())
    }

    pub fn hours(&self) -> Result<Vec<SegmentInfo>> {
        let mut hours: BTreeMap<u32, SegmentInfo> = BTreeMap::new();
        for file in list_files(&self.dir)? {
            let info = hours.entry(file.hour).or_insert(SegmentInfo {
                hour: file.hour,
                sealed: true,
                bytes: 0,
            });
            info.bytes += file.bytes;
            info.sealed &= file.kind == FileKind::Sealed;
        }
        Ok(hours.into_values().collect())
    }

    /// Deletes every file of `hour`, including a live one, without needing the identity.
    pub fn remove_hour(&mut self, hour: u32) -> Result<()> {
        self.live.remove(&hour);
        for file in list_files(&self.dir)?
            .into_iter()
            .filter(|f| f.hour == hour)
        {
            remove_durably(&file.path)?;
        }
        for path in quarantined_files(&self.dir, hour)? {
            remove_durably(&path)?;
        }
        self.forget_table(hour);
        Ok(())
    }

    fn target_hour(&self, now: Ts) -> Result<u32> {
        let wanted = hour_of(now);
        if let Some(&latest) = self.live.keys().next_back()
            && latest >= wanted
        {
            return Ok(latest);
        }
        let after_sealed = list_files(&self.dir)?
            .iter()
            .filter(|file| file.kind == FileKind::Sealed)
            .map(|file| file.hour.saturating_add(1))
            .max()
            .unwrap_or(0);
        Ok(wanted.max(after_sealed))
    }

    fn live_segment(&mut self, hour: u32) -> Result<&mut LiveSegment> {
        if !self.live.contains_key(&hour) {
            let siblings: Vec<SegFile> = list_files(&self.dir)?
                .into_iter()
                .filter(|file| file.hour == hour && file.kind != FileKind::Sealed)
                .collect();
            let segment = LiveSegment::create(&self.dir, hour, &self.recipient, &siblings)?;
            self.live.insert(hour, segment);
        }
        Ok(self.live.get_mut(&hour).expect("inserted above"))
    }

    fn orphans(&self) -> Result<Vec<SegFile>> {
        Ok(list_files(&self.dir)?
            .into_iter()
            .filter(|file| file.kind != FileKind::Sealed)
            .filter(|file| {
                self.live
                    .get(&file.hour)
                    .is_none_or(|live| live.path != file.path)
            })
            .collect())
    }

    fn orphans_of(&self, hour: u32) -> Result<Vec<SegFile>> {
        Ok(self
            .orphans()?
            .into_iter()
            .filter(|f| f.hour == hour)
            .collect())
    }

    fn compact_hour(
        &mut self,
        hour: u32,
        orphans: &[SegFile],
        id: &Identity,
        seal: bool,
    ) -> Result<()> {
        let has_seg = sealed_path(&self.dir, hour).exists();
        if !self.live.contains_key(&hour) && !seal && !has_seg {
            self.live_segment(hour)?;
        }
        if self.live.contains_key(&hour) {
            self.absorb_into_live(hour, orphans, id)
        } else {
            self.seal_orphans(hour, orphans, id)
        }
    }

    /// Quarantines the orphans that cannot be read on their own, then retries the rest.
    fn recover_hour(
        &mut self,
        hour: u32,
        orphans: Vec<SegFile>,
        id: &Identity,
        seal: bool,
        error: VaultError,
        report: &mut CompactReport,
    ) {
        let total = orphans.len();
        let mut readable = Vec::new();
        for orphan in orphans {
            let Err(reason) = check_orphan(&orphan, id) else {
                readable.push(orphan);
                continue;
            };
            match quarantine(&orphan.path) {
                Ok(path) => report.quarantined.push((path, reason)),
                Err(error) => report.failed.push((hour, error)),
            }
        }
        if readable.len() == total {
            report.failed.push((hour, error));
        } else if !readable.is_empty() {
            match self.compact_hour(hour, &readable, id, seal) {
                Ok(()) => report.compacted.push(hour),
                Err(error) => report.failed.push((hour, error)),
            }
        }
    }

    /// Returns false when nothing was written: the segment was empty, or the hour already
    /// has a `.seg` and the file is left behind as an orphan for `compact_orphans`.
    fn seal_live(&self, segment: &LiveSegment) -> Result<bool> {
        if segment.frames.is_empty() {
            remove_durably(&segment.path)?;
            return Ok(false);
        }
        if sealed_path(&self.dir, segment.hour).exists() {
            return Ok(false);
        }
        let mut writer = SealedWriter::create(&self.dir, segment.hour, &self.recipient)?;
        segment.for_each_record(|seq, bytes| writer.add(seq, &bytes))?;
        writer.commit()?;
        self.forget_table(segment.hour);
        remove_durably(&segment.path)?;
        Ok(true)
    }

    fn seal_orphans(&self, hour: u32, orphans: &[SegFile], id: &Identity) -> Result<()> {
        let scans = scan_all(orphans)?;
        let existing = self.open_sealed(hour, id)?;
        let existing_seqs = existing.iter().flat_map(|(_, table)| table.seqs());
        let mut placer = SeqPlacer::above(existing_seqs.chain(frame_seqs(&scans)));
        let mut writer = SealedWriter::create(&self.dir, hour, &self.recipient)?;
        let mut written = 0usize;
        let mut add = |seq: u32, bytes: Zeroizing<Vec<u8>>| -> Result<()> {
            if let Some(seq) = placer.place(seq, &bytes) {
                writer.add(seq, &bytes)?;
                written += 1;
            }
            Ok(())
        };
        let had_seg = existing.is_some();
        if let Some((mut file, table)) = existing {
            for seq in table.seqs() {
                let (offset, len) = table.get(seq).expect("seq comes from table");
                add(seq, file.read_plain(offset, len)?)?;
            }
        }
        for (orphan, scan) in orphans.iter().zip(&scans) {
            let key = scan.unseal(id, hour)?;
            open_file::for_each_record(&orphan.path, &key, hour, &scan.frames, true, &mut add)?;
        }
        if written > 0 || had_seg {
            writer.commit()?;
            self.forget_table(hour);
        }
        remove_all(orphans)
    }

    fn absorb_into_live(&mut self, hour: u32, orphans: &[SegFile], id: &Identity) -> Result<()> {
        let scans = scan_all(orphans)?;
        let segment = self
            .live
            .get_mut(&hour)
            .expect("caller checked hour is live");
        let live_seqs = segment.frames.iter().map(|frame| frame.frame.seq);
        let mut placer = SeqPlacer::above(live_seqs.chain(frame_seqs(&scans)));
        placer.next_free = placer.next_free.max(segment.next_seq);
        segment.for_each_record(|seq, bytes| {
            placer.place(seq, &bytes);
            Ok(())
        })?;
        for (orphan, scan) in orphans.iter().zip(&scans) {
            let key = scan.unseal(id, hour)?;
            open_file::for_each_record(
                &orphan.path,
                &key,
                hour,
                &scan.frames,
                true,
                |seq, bytes| match placer.place(seq, &bytes) {
                    Some(seq) => segment.push(seq, &bytes, decode(&bytes)?.ts),
                    None => Ok(()),
                },
            )?;
        }
        segment.next_seq = segment.next_seq.max(placer.next_free);
        remove_all(orphans)
    }

    fn read_sealed(&self, r: RecordRef, id: &Identity) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let Some((mut file, table)) = self.open_sealed(r.hour, id)? else {
            return Ok(None);
        };
        table
            .get(r.seq)
            .map(|(offset, len)| file.read_plain(offset, len))
            .transpose()
    }

    fn read_orphan(&self, r: RecordRef, id: &Identity) -> Result<Option<Zeroizing<Vec<u8>>>> {
        for orphan in self.orphans_of(r.hour)? {
            let scan = open_file::scan(&orphan.path)?;
            let Some(frame) = scan.find(r.seq) else {
                continue;
            };
            let key = scan.unseal(id, r.hour)?;
            match open_file::read_record(&orphan.path, &key, r.hour, frame)? {
                Some(bytes) => return Ok(Some(bytes)),
                None if scan.frames.last() == Some(&frame) => {}
                None => return Err(corrupt("segment record failed authentication")),
            }
        }
        Ok(None)
    }

    fn open_sealed(
        &self,
        hour: u32,
        id: &Identity,
    ) -> Result<Option<(SealedFile, Arc<SealedTable>)>> {
        let path = sealed_path(&self.dir, hour);
        if !path.exists() {
            return Ok(None);
        }
        let mut file = SealedFile::open(&path, hour, id)?;
        let cached = self.lock_tables().get(&hour).cloned();
        let table = match cached {
            Some(table) => table,
            None => {
                let table = Arc::new(file.read_table()?);
                self.lock_tables().insert(hour, table.clone());
                table
            }
        };
        Ok(Some((file, table)))
    }

    fn forget_table(&self, hour: u32) {
        self.lock_tables().remove(&hour);
    }

    fn lock_tables(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Arc<SealedTable>>> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The open file this process writes to, with its key and each record's timestamp.
struct LiveSegment {
    hour: u32,
    path: PathBuf,
    key: HourKey,
    header_len: u64,
    frames: Vec<LiveFrame>,
    next_seq: u32,
}

struct LiveFrame {
    frame: Frame,
    ts: Ts,
}

impl LiveSegment {
    /// Starts a new file next to the hour's orphans, with seqs above all of theirs.
    fn create(dir: &Path, hour: u32, recipient: &Recipient, siblings: &[SegFile]) -> Result<Self> {
        let taken: HashSet<FileKind> = siblings.iter().map(|file| file.kind).collect();
        let suffix = (0..)
            .find(|&suffix| !taken.contains(&FileKind::Open { suffix }))
            .expect("a free suffix exists");
        let scans: Vec<_> = siblings
            .iter()
            .filter_map(|file| open_file::scan(&file.path).ok())
            .collect();
        let path = open_path(dir, hour, suffix);
        let (key, header_len) = open_file::create(&path, hour, recipient)?;
        Ok(Self {
            hour,
            path,
            key,
            header_len,
            frames: Vec::new(),
            next_seq: SeqPlacer::above(frame_seqs(&scans)).next_free,
        })
    }

    fn end(&self) -> u64 {
        self.frames
            .last()
            .map_or(self.header_len, |live| live.frame.end())
    }

    fn append(&mut self, plaintext: &[u8], ts: Ts) -> Result<u32> {
        let seq = self.next_seq;
        let next = seq
            .checked_add(1)
            .ok_or_else(|| corrupt("hour segment is full"))?;
        self.push(seq, plaintext, ts)?;
        self.next_seq = next;
        Ok(seq)
    }

    fn push(&mut self, seq: u32, plaintext: &[u8], ts: Ts) -> Result<()> {
        let bytes = open_file::seal_frame(&self.key, self.hour, seq, plaintext)?;
        let offset = self.end();
        open_file::append_frame(&self.path, self.hour, offset, &bytes)?;
        let frame = Frame {
            seq,
            offset,
            len: bytes.len() as u64,
        };
        self.frames.push(LiveFrame { frame, ts });
        Ok(())
    }

    fn read(&self, seq: u32) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(live) = self.frames.iter().find(|live| live.frame.seq == seq) else {
            return Ok(None);
        };
        open_file::read_record(&self.path, &self.key, self.hour, live.frame)?
            .ok_or_else(|| corrupt("segment record failed authentication"))
            .map(Some)
    }

    fn frames(&self) -> Vec<Frame> {
        self.frames.iter().map(|live| live.frame).collect()
    }

    /// Visits records in seq order.
    fn for_each_record(
        &self,
        visit: impl FnMut(u32, Zeroizing<Vec<u8>>) -> Result<()>,
    ) -> Result<()> {
        let mut frames = self.frames();
        frames.sort_by_key(|frame| frame.seq);
        open_file::for_each_record(&self.path, &self.key, self.hour, &frames, false, visit)
    }

    /// Rewrites the file without the frames `keep` rejects. Returns how many were dropped.
    fn retain(&mut self, keep: impl Fn(&LiveFrame) -> bool) -> Result<u32> {
        let kept: HashMap<u32, Ts> = self
            .frames
            .iter()
            .filter(|live| keep(live))
            .map(|live| (live.frame.seq, live.ts))
            .collect();
        let dropped = self.frames.len() - kept.len();
        if dropped == 0 {
            return Ok(0);
        }
        let moved = open_file::rewrite_keeping(
            &self.path,
            self.hour,
            self.header_len,
            &self.frames(),
            |frame| kept.contains_key(&frame.seq),
        )?;
        self.frames = moved
            .into_iter()
            .map(|frame| LiveFrame {
                ts: kept[&frame.seq],
                frame,
            })
            .collect();
        Ok(dropped as u32)
    }
}

/// Chooses the seq each merged record is stored under.
struct SeqPlacer {
    by_seq: HashMap<u32, [u8; 32]>,
    placed: HashSet<[u8; 32]>,
    next_free: u32,
}

impl SeqPlacer {
    fn above(seqs: impl Iterator<Item = u32>) -> Self {
        Self {
            by_seq: HashMap::new(),
            placed: HashSet::new(),
            next_free: seqs.max().map_or(0, |max| max.saturating_add(1)),
        }
    }

    /// `None` when the record is already placed: a seal that crashed between writing the
    /// `.seg` and removing the `.open` leaves both behind, and a compaction retried after
    /// a failure finds the records it already absorbed under their (maybe renumbered) seq.
    fn place(&mut self, seq: u32, record: &[u8]) -> Option<u32> {
        let digest: [u8; 32] = Sha256::digest(record).into();
        let placed_seq = match self.by_seq.get(&seq) {
            None => seq,
            Some(existing) if *existing == digest => return None,
            Some(_) if self.placed.contains(&digest) => return None,
            Some(_) => {
                let renumbered = self.next_free;
                self.next_free = self.next_free.saturating_add(1);
                renumbered
            }
        };
        self.by_seq.insert(placed_seq, digest);
        self.placed.insert(digest);
        Some(placed_seq)
    }
}

fn is_due(hour: u32, now: Ts, grace_mins: u32) -> bool {
    now >= hour_end_ms(hour) + i64::from(grace_mins) * 60_000
}

/// Reads every record of an orphan, to tell a bad file apart from a failing hour.
fn check_orphan(orphan: &SegFile, id: &Identity) -> Result<()> {
    let scan = open_file::scan(&orphan.path)?;
    let key = scan.unseal(id, orphan.hour)?;
    open_file::for_each_record(
        &orphan.path,
        &key,
        orphan.hour,
        &scan.frames,
        true,
        |_, bytes| decode(&bytes).map(drop),
    )
}

fn scan_all(files: &[SegFile]) -> Result<Vec<open_file::Scan>> {
    files
        .iter()
        .map(|file| open_file::scan(&file.path))
        .collect()
}

fn frame_seqs(scans: &[open_file::Scan]) -> impl Iterator<Item = u32> + '_ {
    scans
        .iter()
        .flat_map(|scan| scan.frames.iter().map(|frame| frame.seq))
}

fn remove_all(files: &[SegFile]) -> Result<()> {
    files.iter().try_for_each(|file| remove_durably(&file.path))
}

fn encode(record: &Record) -> Result<Zeroizing<Vec<u8>>> {
    postcard::to_stdvec(record)
        .map(Zeroizing::new)
        .map_err(corrupt)
}

fn decode(bytes: &[u8]) -> Result<Record> {
    postcard::from_bytes(bytes).map_err(corrupt)
}
