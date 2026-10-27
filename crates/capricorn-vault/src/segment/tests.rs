use std::fs;
use std::path::{Path, PathBuf};

use capricorn_core::proto::Kind;

use super::*;
use crate::keyfile::{KdfParams, Vault};

const H: u32 = 480_000;
const GRACE: u32 = 10;

fn at(hour: u32, minute: i64) -> Ts {
    hour_start_ms(hour) + minute * 60_000
}

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    id: Identity,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let vault_dir = tmp.path().join("vault");
        fs::create_dir(&vault_dir).unwrap();
        let (_, id, _) =
            Vault::create(&vault_dir, "pw", KdfParams::insecure_fast(), false).unwrap();
        let dir = tmp.path().join("segments");
        Self { _tmp: tmp, dir, id }
    }

    fn store(&self) -> SegmentStore {
        SegmentStore::open(&self.dir, self.id.recipient()).unwrap()
    }

    fn file_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn rec(ts: Ts, text: &str) -> Record {
    Record {
        ts,
        kind: Kind::Screen,
        text: text.into(),
        ..Record::default()
    }
}

fn append_at(store: &mut SegmentStore, ts: Ts, text: &str) -> (RecordRef, Record) {
    let record = rec(ts, text);
    (store.append(&record, ts).unwrap(), record)
}

/// Compacts and asserts nothing was quarantined or failed.
fn compacted(store: &mut SegmentStore, fx: &Fixture, now: Ts) -> Vec<u32> {
    let report = store.compact_orphans(&fx.id, now, GRACE).unwrap();
    assert!(report.quarantined.is_empty(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    report.compacted
}

fn mtime(path: &Path) -> std::time::SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn texts(records: &[(RecordRef, Record)]) -> Vec<&str> {
    records.iter().map(|(_, r)| r.text.as_str()).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn assert_no_canaries(dir: &Path, canaries: &[&[u8]]) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = fs::read(&path).unwrap();
        for canary in canaries {
            assert!(
                !contains(&bytes, canary),
                "{} leaks plaintext",
                path.display()
            );
        }
    }
}

#[test]
fn append_read_while_locked_current_hour() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let written: Vec<_> = (0..3)
        .map(|i| append_at(&mut store, at(H, 10 + i), &format!("rec {i}")))
        .collect();
    for (r, record) in &written {
        assert_eq!(r.hour, H);
        assert_eq!(&store.read(*r, None).unwrap(), record);
    }
    let seqs: Vec<u32> = written.iter().map(|(r, _)| r.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[test]
fn sealed_hour_unreadable_without_identity() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (r, record) = append_at(&mut store, at(H, 30), "hello");
    assert!(
        store
            .seal_due(at(H + 1, GRACE as i64 - 1), GRACE)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.seal_due(at(H + 1, GRACE as i64), GRACE).unwrap(),
        vec![H]
    );
    assert!(matches!(store.read(r, None), Err(VaultError::Locked)));
    assert_eq!(store.read(r, Some(&fx.id)).unwrap(), record);
    assert_eq!(fx.file_names(), vec![format!("{H}.seg")]);
    let info = store.hours().unwrap();
    assert_eq!(info.len(), 1);
    assert!(info[0].sealed);
}

#[test]
fn no_plaintext_on_disk() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let mut record = rec(at(H, 5), "secret CANARY-7f3a text");
    record.image = Some(b"webp....CANARY-IMG....".to_vec());
    record.thumb = Some(b"thumbCANARY-IMG".to_vec());
    record.window.title = "CANARY-7f3a title".into();
    let r = store.append(&record, record.ts).unwrap();
    let canaries: [&[u8]; 2] = [b"CANARY-7f3a", b"CANARY-IMG"];
    assert_no_canaries(&fx.dir, &canaries);
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    assert_no_canaries(&fx.dir, &canaries);
    assert_eq!(store.read(r, Some(&fx.id)).unwrap(), record);
}

#[test]
fn forget_since_removes_recent_only() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (a, _) = append_at(&mut store, at(H, 10), "a");
    let (b, _) = append_at(&mut store, at(H, 20), "b");
    let (c, _) = append_at(&mut store, at(H, 30), "c");
    assert_eq!(store.forget_since(at(H, 25)).unwrap(), vec![c]);
    assert_eq!(store.read(a, None).unwrap().text, "a");
    assert_eq!(store.read(b, None).unwrap().text, "b");
    assert!(matches!(
        store.read(c, Some(&fx.id)),
        Err(VaultError::NotFound(_))
    ));
    let (d, _) = append_at(&mut store, at(H, 40), "d");
    assert_eq!(d.seq, 3);
    assert_eq!(
        texts(&store.records_in(H, &fx.id).unwrap()),
        vec!["a", "b", "d"]
    );
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    assert_eq!(
        texts(&store.records_in(H, &fx.id).unwrap()),
        vec!["a", "b", "d"]
    );
}

#[test]
fn forget_reaches_previous_hour_within_grace() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (kept, _) = append_at(&mut store, at(H, 50), "kept");
    let (late, _) = append_at(&mut store, at(H, 59), "late");
    let (next, _) = append_at(&mut store, at(H + 1, 2), "next");
    assert_eq!(next.hour, H + 1);
    assert!(store.seal_due(at(H + 1, 3), GRACE).unwrap().is_empty());
    assert_eq!(store.forget_since(at(H, 58)).unwrap(), vec![late, next]);
    assert!(store.read(late, Some(&fx.id)).is_err());
    assert!(store.read(next, Some(&fx.id)).is_err());
    assert_eq!(store.read(kept, None).unwrap().text, "kept");
    assert_eq!(store.seal_due(at(H + 3, 0), GRACE).unwrap(), vec![H]);
    assert_eq!(texts(&store.records_in(H, &fx.id).unwrap()), vec!["kept"]);
    assert!(store.records_in(H + 1, &fx.id).unwrap().is_empty());
}

#[test]
fn orphan_open_segment_compacts_on_unlock() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let (a, _) = append_at(&mut first, at(H, 1), "a");
    let (b, _) = append_at(&mut first, at(H, 2), "b");
    drop(first);

    let mut store = fx.store();
    assert!(matches!(store.read(a, None), Err(VaultError::Locked)));
    assert_eq!(store.read(a, Some(&fx.id)).unwrap().text, "a");
    let (c, _) = append_at(&mut store, at(H, 3), "c");
    assert_eq!(c.hour, H);
    assert!(c.seq > b.seq);
    assert_eq!(
        fx.file_names(),
        vec![format!("{H}.1.open"), format!("{H}.open")]
    );
    assert!(!store.hours().unwrap()[0].sealed);

    assert_eq!(compacted(&mut store, &fx, at(H, 3)), vec![H]);
    assert_eq!(fx.file_names(), vec![format!("{H}.1.open")]);
    for (r, text) in [(a, "a"), (b, "b"), (c, "c")] {
        assert_eq!(store.read(r, None).unwrap().text, text);
    }
    let (d, _) = append_at(&mut store, at(H, 4), "d");
    let refs: HashSet<RecordRef> = [a, b, c, d].into_iter().collect();
    assert_eq!(refs.len(), 4);

    assert_eq!(store.seal_due(at(H + 2, 0), GRACE).unwrap(), vec![H]);
    let all = store.records_in(H, &fx.id).unwrap();
    assert_eq!(texts(&all), vec!["a", "b", "c", "d"]);
    assert_eq!(all.iter().map(|(r, _)| *r).collect::<HashSet<_>>(), refs);
}

#[test]
fn orphans_of_past_hour_compact_into_sealed_segment() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let (a, _) = append_at(&mut first, at(H, 1), "a");
    drop(first);
    let mut store = fx.store();
    append_at(&mut store, at(H + 3, 0), "later");
    assert_eq!(compacted(&mut store, &fx, at(H + 3, 0)), vec![H]);
    assert!(fx.file_names().contains(&format!("{H}.seg")));
    assert!(!fx.file_names().contains(&format!("{H}.open")));
    assert!(matches!(store.read(a, None), Err(VaultError::Locked)));
    assert_eq!(store.read(a, Some(&fx.id)).unwrap().text, "a");
    assert!(compacted(&mut store, &fx, at(H + 3, 0)).is_empty());
}

#[test]
fn interrupted_seal_does_not_duplicate_records() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H, 1), "a");
    append_at(&mut first, at(H, 2), "b");
    let open = fx.dir.join(format!("{H}.open"));
    let copy = fs::read(&open).unwrap();
    first.seal_due(at(H + 2, 0), GRACE).unwrap();
    drop(first);
    fs::write(&open, copy).unwrap();

    let mut store = fx.store();
    assert_eq!(compacted(&mut store, &fx, at(H + 2, 0)), vec![H]);
    let all = store.records_in(H, &fx.id).unwrap();
    assert_eq!(texts(&all), vec!["a", "b"]);
    assert_eq!(fx.file_names(), vec![format!("{H}.seg")]);
}

#[test]
fn forget_deletes_orphans_that_could_overlap() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H - 3, 10), "old");
    append_at(&mut first, at(H, 10), "x");
    append_at(&mut first, at(H, 20), "y");
    drop(first);

    let mut store = fx.store();
    assert_eq!(store.forget_since(at(H, 55)).unwrap().len(), 2);
    assert_eq!(
        fx.file_names(),
        vec![format!("{}.open", H - 3), format!("{H}.floor")]
    );
}

#[test]
fn out_of_order_ts_stays_in_open_segment() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (first, _) = append_at(&mut store, at(H + 1, 5), "first");
    let earlier = rec(at(H, 30), "earlier");
    let r = store.append(&earlier, at(H, 31)).unwrap();
    assert_eq!(r.hour, H + 1);
    assert_eq!(r.seq, first.seq + 1);
    assert_eq!(store.read(r, None).unwrap().ts, at(H, 30));

    let ahead = rec(at(H + 2, 1), "ahead");
    let r_ahead = store.append(&ahead, at(H + 1, 59)).unwrap();
    assert_eq!(r_ahead.hour, H + 1);
}

#[test]
fn clock_going_back_after_restart_never_reopens_sealed_hour() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let (a, _) = append_at(&mut first, at(H, 5), "a");
    first.seal_due(at(H + 1, 30), GRACE).unwrap();
    drop(first);

    let mut store = fx.store();
    let (b, _) = append_at(&mut store, at(H, 40), "b");
    assert_eq!(b.hour, H + 1);
    store.seal_due(at(H + 3, 0), GRACE).unwrap();
    assert_eq!(store.read(a, Some(&fx.id)).unwrap().text, "a");
    assert_eq!(store.read(b, Some(&fx.id)).unwrap().ts, at(H, 40));
}

#[test]
fn torn_tail_is_ignored_on_reopen() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let refs: Vec<RecordRef> = (0..3)
        .map(|i| append_at(&mut first, at(H, i), &format!("r{i}")).0)
        .collect();
    drop(first);
    let open = fx.dir.join(format!("{H}.open"));
    let len = fs::metadata(&open).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&open)
        .unwrap()
        .set_len(len - 10)
        .unwrap();

    let mut store = fx.store();
    assert_eq!(store.read(refs[0], Some(&fx.id)).unwrap().text, "r0");
    assert_eq!(store.read(refs[1], Some(&fx.id)).unwrap().text, "r1");
    assert!(matches!(
        store.read(refs[2], Some(&fx.id)),
        Err(VaultError::NotFound(_))
    ));
    let (next, _) = append_at(&mut store, at(H, 30), "next");
    assert_eq!(store.read(next, None).unwrap().text, "next");
    compacted(&mut store, &fx, at(H, 31));
    assert_eq!(
        texts(&store.records_in(H, &fx.id).unwrap()),
        vec!["r0", "r1", "next"]
    );
}

#[test]
fn corrupt_final_frame_of_orphan_is_ignored() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H, 1), "r0");
    let (r1, _) = append_at(&mut first, at(H, 2), "r1");
    drop(first);
    let open = fx.dir.join(format!("{H}.open"));
    let mut bytes = fs::read(&open).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&open, bytes).unwrap();

    let mut store = fx.store();
    assert!(matches!(
        store.read(r1, Some(&fx.id)),
        Err(VaultError::NotFound(_))
    ));
    compacted(&mut store, &fx, at(H + 2, 0));
    assert_eq!(texts(&store.records_in(H, &fx.id).unwrap()), vec!["r0"]);
}

#[test]
fn failed_append_leftovers_are_cut_on_next_append() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (a, _) = append_at(&mut store, at(H, 1), "a");
    let open = fx.dir.join(format!("{H}.open"));
    let mut file = fs::OpenOptions::new().append(true).open(&open).unwrap();
    std::io::Write::write_all(&mut file, &[0, 0, 1, 0, 9, 9, 9]).unwrap();
    drop(file);
    let (b, _) = append_at(&mut store, at(H, 2), "b");
    assert_eq!(store.read(a, None).unwrap().text, "a");
    assert_eq!(store.read(b, None).unwrap().text, "b");
    drop(store);

    let mut store = fx.store();
    compacted(&mut store, &fx, at(H + 2, 0));
    assert_eq!(texts(&store.records_in(H, &fx.id).unwrap()), vec!["a", "b"]);
}

#[test]
fn delete_records_rewrites_segment() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let refs: Vec<RecordRef> = (0..4)
        .map(|i| {
            let mut record = rec(at(H, i), &format!("r{i}"));
            record.image = Some(vec![7; 4096]);
            store.append(&record, record.ts).unwrap()
        })
        .collect();
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    let seg = fx.dir.join(format!("{H}.seg"));
    let before = fs::read(&seg).unwrap();
    assert_eq!(store.read(refs[1], Some(&fx.id)).unwrap().text, "r1");

    store.delete_records(H, &[1, 2, 99], &fx.id).unwrap();
    for gone in [refs[1], refs[2]] {
        assert!(
            matches!(store.read(gone, Some(&fx.id)), Err(VaultError::NotFound(r)) if r == gone)
        );
    }
    assert_eq!(store.read(refs[0], Some(&fx.id)).unwrap().text, "r0");
    assert_eq!(store.read(refs[3], Some(&fx.id)).unwrap().text, "r3");
    let after = fs::read(&seg).unwrap();
    assert!(after.len() < before.len());
    assert_ne!(after[..64], before[..64]);
    let remaining: Vec<RecordRef> = store
        .records_in(H, &fx.id)
        .unwrap()
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(remaining, vec![refs[0], refs[3]]);
}

#[test]
fn delete_records_in_open_hour_and_orphan() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let (orphaned, _) = append_at(&mut first, at(H, 1), "orphaned");
    drop(first);
    let mut store = fx.store();
    let (live, _) = append_at(&mut store, at(H, 2), "live");
    let (stays, _) = append_at(&mut store, at(H, 3), "stays");
    store
        .delete_records(H, &[orphaned.seq, live.seq], &fx.id)
        .unwrap();
    assert!(matches!(store.read(live, None), Err(VaultError::Locked)));
    assert!(matches!(
        store.read(orphaned, Some(&fx.id)),
        Err(VaultError::NotFound(_))
    ));
    assert_eq!(store.read(stays, None).unwrap().text, "stays");
}

#[test]
fn tampered_sealed_segment_errors() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (r, _) = append_at(&mut store, at(H, 1), "r");
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    let seg = fx.dir.join(format!("{H}.seg"));
    let mut bytes = fs::read(&seg).unwrap();
    let middle = bytes.len() - 20;
    bytes[middle] ^= 1;
    fs::write(&seg, bytes).unwrap();
    let store = fx.store();
    assert!(matches!(
        store.read(r, Some(&fx.id)),
        Err(VaultError::Corrupt(_))
    ));
}

#[test]
fn renamed_file_does_not_open_as_another_hour() {
    let fx = Fixture::new();
    let mut store = fx.store();
    append_at(&mut store, at(H, 1), "r");
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    fs::rename(
        fx.dir.join(format!("{H}.seg")),
        fx.dir.join(format!("{}.seg", H + 7)),
    )
    .unwrap();
    let store = fx.store();
    let moved = RecordRef {
        hour: H + 7,
        seq: 0,
    };
    assert!(store.read(moved, Some(&fx.id)).is_err());
}

#[test]
fn hours_and_remove_hour() {
    let fx = Fixture::new();
    let mut store = fx.store();
    append_at(&mut store, at(H, 1), "a");
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    append_at(&mut store, at(H + 2, 1), "b");
    let hours = store.hours().unwrap();
    assert_eq!(
        hours.iter().map(|h| (h.hour, h.sealed)).collect::<Vec<_>>(),
        vec![(H, true), (H + 2, false)]
    );
    assert!(hours.iter().all(|h| h.bytes > 0));

    store.remove_hour(H).unwrap();
    store.remove_hour(H + 2).unwrap();
    assert!(store.hours().unwrap().is_empty());
    assert!(fx.file_names().is_empty());
    let (c, _) = append_at(&mut store, at(H + 2, 5), "c");
    assert_eq!(store.read(c, None).unwrap().text, "c");
}

#[test]
fn empty_open_hour_seals_to_nothing() {
    let fx = Fixture::new();
    let mut store = fx.store();
    append_at(&mut store, at(H, 1), "a");
    store.forget_since(at(H, 0)).unwrap();
    assert!(store.seal_due(at(H + 2, 0), GRACE).unwrap().is_empty());
    assert!(fx.file_names().is_empty());
}

#[test]
fn stale_temp_files_are_removed_on_open() {
    let fx = Fixture::new();
    drop(fx.store());
    fs::write(fx.dir.join(format!("{H}.abc123.tmp")), b"ciphertext").unwrap();
    drop(fx.store());
    assert!(fx.file_names().is_empty());
}

#[test]
fn store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SegmentStore>();
}

#[test]
fn compact_keeps_hours_within_grace_open() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H, 58), "late");
    append_at(&mut first, at(H + 1, 1), "early");
    drop(first);

    let mut store = fx.store();
    let now = at(H + 1, 2);
    let mut compacted_hours = compacted(&mut store, &fx, now);
    compacted_hours.sort();
    assert_eq!(compacted_hours, vec![H, H + 1]);
    assert!(store.hours().unwrap().iter().all(|info| !info.sealed));
    assert_eq!(store.forget_since(now - 5 * 60_000).unwrap().len(), 2);
    let (next, _) = append_at(&mut store, at(H + 1, 3), "next");
    assert_eq!(next.hour, H + 1);
    assert_eq!(store.read(next, None).unwrap().text, "next");
    assert!(store.records_in(H, &fx.id).unwrap().is_empty());
    assert_eq!(
        texts(&store.records_in(H + 1, &fx.id).unwrap()),
        vec!["next"]
    );
}

#[test]
fn compact_absorbed_hour_seals_later() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let (a, _) = append_at(&mut first, at(H, 58), "a");
    drop(first);
    let mut store = fx.store();
    assert_eq!(compacted(&mut store, &fx, at(H + 1, 2)), vec![H]);
    assert_eq!(store.read(a, None).unwrap().text, "a");
    assert_eq!(store.seal_due(at(H + 2, 0), GRACE).unwrap(), vec![H]);
    assert_eq!(fx.file_names(), vec![format!("{H}.seg")]);
    assert_eq!(store.read(a, Some(&fx.id)).unwrap().text, "a");
}

#[test]
fn far_future_ts_does_not_pin_the_bucket() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let future = rec(i64::MAX, "future");
    let r = store.append(&future, at(H, 1)).unwrap();
    assert_eq!(r.hour, H);
    assert_eq!(store.read(r, None).unwrap().ts, i64::MAX);
    let (next, _) = append_at(&mut store, at(H + 1, 1), "next");
    assert_eq!(next.hour, H + 1);
    assert_eq!(store.seal_due(at(H + 1, 30), GRACE).unwrap(), vec![H]);
    assert_eq!(store.read(r, Some(&fx.id)).unwrap().ts, i64::MAX);
}

#[test]
fn bad_orphan_is_quarantined_and_others_compact() {
    let fx = Fixture::new();
    let mut first = fx.store();
    let (good, _) = append_at(&mut first, at(H, 1), "good");
    let (lost, _) = append_at(&mut first, at(H + 1, 1), "lost");
    drop(first);
    let bad_path = fx.dir.join(format!("{}.open", H + 1));
    let mut bytes = fs::read(&bad_path).unwrap();
    bytes[20] ^= 0xff;
    fs::write(&bad_path, bytes).unwrap();

    let mut store = fx.store();
    let report = store.compact_orphans(&fx.id, at(H + 3, 0), GRACE).unwrap();
    assert_eq!(report.compacted, vec![H]);
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.quarantined.len(), 1);
    let quarantined = &report.quarantined[0].0;
    assert_eq!(
        quarantined.file_name().unwrap().to_str().unwrap(),
        format!("{}.open.bad", H + 1)
    );
    assert_eq!(store.read(good, Some(&fx.id)).unwrap().text, "good");
    assert!(matches!(
        store.read(lost, Some(&fx.id)),
        Err(VaultError::NotFound(_))
    ));
    assert_eq!(store.hours().unwrap().len(), 1);

    let report = store.compact_orphans(&fx.id, at(H + 3, 0), GRACE).unwrap();
    assert!(report.compacted.is_empty() && report.quarantined.is_empty());
    store.remove_hour(H + 1).unwrap();
    assert_eq!(fx.file_names(), vec![format!("{H}.seg")]);
}

#[test]
fn corrupt_middle_frame_quarantines_only_that_file() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H, 1), "r0");
    append_at(&mut first, at(H, 2), "r1");
    drop(first);
    let mut second = fx.store();
    let (kept, _) = append_at(&mut second, at(H, 3), "kept");
    drop(second);
    let broken = fx.dir.join(format!("{H}.open"));
    let mut bytes = fs::read(&broken).unwrap();
    let first_frame = open_file::scan(&broken).unwrap().frames[0];
    bytes[first_frame.offset as usize + 40] ^= 1;
    fs::write(&broken, bytes).unwrap();

    let mut store = fx.store();
    let report = store.compact_orphans(&fx.id, at(H + 3, 0), GRACE).unwrap();
    assert_eq!(report.compacted, vec![H]);
    assert_eq!(report.quarantined.len(), 1);
    assert_eq!(texts(&store.records_in(H, &fx.id).unwrap()), vec!["kept"]);
    assert_eq!(store.read(kept, Some(&fx.id)).unwrap().text, "kept");
}

#[test]
fn file_times_show_only_the_hour() {
    let fx = Fixture::new();
    let mut store = fx.store();
    append_at(&mut store, at(H, 42), "a");
    let hour_start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(H) * 3600);
    assert_eq!(mtime(&fx.dir.join(format!("{H}.open"))), hour_start);
    append_at(&mut store, at(H, 43), "b");
    store.forget_since(at(H, 43)).unwrap();
    assert_eq!(mtime(&fx.dir.join(format!("{H}.open"))), hour_start);
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    assert_eq!(mtime(&fx.dir.join(format!("{H}.seg"))), hour_start);
}

#[test]
fn hostile_sealed_length_is_corrupt_not_panic() {
    let fx = Fixture::new();
    let mut store = fx.store();
    let (r, _) = append_at(&mut store, at(H, 1), "r");
    store.seal_due(at(H + 2, 0), GRACE).unwrap();
    let seg = fx.dir.join(format!("{H}.seg"));
    let original = fs::read(&seg).unwrap();
    let blob_len = u32::from_be_bytes(original[9..13].try_into().unwrap()) as usize;
    let len_at = 13 + blob_len;
    let real_len = u64::from_be_bytes(original[len_at..len_at + 8].try_into().unwrap());
    for claimed in [u64::MAX, u64::MAX - 7, 1 << 50, real_len + 1, 7] {
        let mut bytes = original.clone();
        bytes[len_at..len_at + 8].copy_from_slice(&claimed.to_be_bytes());
        fs::write(&seg, bytes).unwrap();
        let store = fx.store();
        assert!(matches!(
            store.read(r, Some(&fx.id)),
            Err(VaultError::Corrupt(_))
        ));
    }
}

#[test]
fn forgotten_orphan_seqs_are_not_reused() {
    let fx = Fixture::new();
    let mut first = fx.store();
    for minute in 1..=3 {
        append_at(&mut first, at(H, minute), "old");
    }
    drop(first);

    let mut store = fx.store();
    let forgotten = store.forget_since(at(H, 0)).unwrap();
    assert_eq!(forgotten.len(), 3);
    let (fresh, _) = append_at(&mut store, at(H, 10), "fresh");
    assert_eq!(fresh.seq, 3);
    drop(store);

    let mut again = fx.store();
    let (later, _) = append_at(&mut again, at(H, 11), "later");
    assert_eq!(later.seq, 4);
}

#[test]
fn forgotten_live_seqs_are_not_reused_after_restart() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H, 1), "kept");
    append_at(&mut first, at(H, 2), "gone");
    append_at(&mut first, at(H, 3), "gone too");
    assert_eq!(first.forget_since(at(H, 2)).unwrap().len(), 2);
    assert_eq!(first.next_seq(H), Some(3));
    assert_eq!(first.next_seq(H + 1), None);
    drop(first);

    let mut store = fx.store();
    let (fresh, _) = append_at(&mut store, at(H, 10), "fresh");
    assert_eq!(fresh.seq, 3);
    compacted(&mut store, &fx, at(H, 11));
    let refs: Vec<u32> = store
        .records_in(H, &fx.id)
        .unwrap()
        .into_iter()
        .map(|(r, _)| r.seq)
        .collect();
    assert_eq!(refs, vec![0, 3]);
    store.remove_hour(H).unwrap();
    assert!(!fx.file_names().iter().any(|name| name.ends_with(".floor")));
}

#[test]
fn unscannable_sibling_is_quarantined_not_reused() {
    let fx = Fixture::new();
    let mut first = fx.store();
    append_at(&mut first, at(H, 1), "a");
    drop(first);
    fs::write(fx.dir.join(format!("{H}.open")), b"garbage").unwrap();

    let mut store = fx.store();
    append_at(&mut store, at(H, 2), "b");
    assert!(fx.file_names().contains(&format!("{H}.open.bad")));
}
