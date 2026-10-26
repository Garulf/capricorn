use std::time::{SystemTime, UNIX_EPOCH};

use capricorn_core::Record;
use capricorn_core::proto::{Kind, RecordRef, SearchQuery, WindowContext};

use crate::{Index, Watermark};

const HOUR_MS: i64 = 3_600_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn rec(ts: i64, kind: Kind, class: &str, workspace: &str, text: &str) -> Record {
    Record {
        ts,
        kind,
        window: WindowContext {
            class: class.into(),
            title: format!("{class} window"),
            workspace: workspace.into(),
            ..WindowContext::default()
        },
        text: text.into(),
        ..Record::default()
    }
}

fn screen(ts: i64, text: &str) -> Record {
    rec(ts, Kind::Screen, "kitty", "1", text)
}

fn r(hour: u32, seq: u32) -> RecordRef {
    RecordRef { hour, seq }
}

fn query(text: &str) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        kind: None,
        app: None,
        workspace: None,
        from: None,
        to: None,
        limit: 20,
    }
}

fn index_with(records: &[(RecordRef, Record)]) -> Index {
    let mut index = Index::new().unwrap();
    for (record_ref, record) in records {
        index.add(*record_ref, record).unwrap();
    }
    index.commit().unwrap();
    index
}

fn refs(index: &Index, q: &SearchQuery) -> Vec<RecordRef> {
    index.search(q).unwrap().into_iter().map(|h| h.r).collect()
}

#[test]
fn finds_exact_words() {
    let index = index_with(&[
        (r(1, 0), screen(now_ms(), "updating the flake lock")),
        (r(1, 1), screen(now_ms(), "unrelated terminal output")),
    ]);
    assert_eq!(refs(&index, &query("flake")), vec![r(1, 0)]);
    assert_eq!(refs(&index, &query("flake lock")), vec![r(1, 0)]);
    assert!(refs(&index, &query("flake banana")).is_empty());
}

#[test]
fn typo_tolerant() {
    let index = index_with(&[(r(1, 0), screen(now_ms(), "nix flake update"))]);
    assert_eq!(refs(&index, &query("flkae")), vec![r(1, 0)]);
}

#[test]
fn prefix_match() {
    let index = index_with(&[(r(1, 0), screen(now_ms(), "inputs.nixpkgs.url"))]);
    assert_eq!(refs(&index, &query("nixp")), vec![r(1, 0)]);
}

#[test]
fn filters_kind_app_workspace_range() {
    let now = now_ms();
    let index = index_with(&[
        (
            r(1, 0),
            rec(now - 3000, Kind::Screen, "kitty", "1", "shared"),
        ),
        (
            r(1, 1),
            rec(now - 2000, Kind::Clipboard, "firefox", "2", "shared"),
        ),
        (
            r(1, 2),
            rec(now - 1000, Kind::Screen, "Firefox", "2", "shared"),
        ),
    ]);
    let with = |f: &dyn Fn(&mut SearchQuery)| {
        let mut q = query("shared");
        f(&mut q);
        let mut found = refs(&index, &q);
        found.sort();
        found
    };
    assert_eq!(with(&|q| q.kind = Some(Kind::Clipboard)), vec![r(1, 1)]);
    assert_eq!(
        with(&|q| q.app = Some("firefox".into())),
        vec![r(1, 1), r(1, 2)]
    );
    assert_eq!(with(&|q| q.workspace = Some("1".into())), vec![r(1, 0)]);
    assert_eq!(
        with(&|q| {
            q.from = Some(now - 2500);
            q.to = Some(now - 1500);
        }),
        vec![r(1, 1)]
    );
}

#[test]
fn recency_breaks_ties() {
    let now = now_ms();
    let index = index_with(&[
        (r(1, 0), screen(now - 3 * 24 * HOUR_MS, "same words here")),
        (r(1, 1), screen(now, "same words here")),
        (r(1, 2), screen(now - 24 * HOUR_MS, "same words here")),
    ]);
    assert_eq!(
        refs(&index, &query("words")),
        vec![r(1, 1), r(1, 2), r(1, 0)]
    );
}

#[test]
fn snippet_highlights_match() {
    let index = index_with(&[(
        r(1, 0),
        screen(now_ms(), "one two three the quick brownfox jumps over"),
    )]);
    let hits = index.search(&query("quick")).unwrap();
    let hit = &hits[0];
    assert!(!hit.highlights.is_empty());
    let (start, end) = hit.highlights[0];
    assert_eq!(&hit.snippet[start as usize..end as usize], "quick");

    let typo = index.search(&query("quikc")).unwrap();
    let (start, end) = typo[0].highlights[0];
    assert_eq!(&typo[0].snippet[start as usize..end as usize], "quick");
}

#[test]
fn snapshot_roundtrip_preserves_hits_and_watermark() {
    let now = now_ms();
    let mut index = index_with(&[
        (r(7, 0), screen(now - 1000, "alpha beta")),
        (r(7, 1), screen(now, "alpha gamma")),
    ]);
    let wm = Watermark {
        sealed_hours: [3, 4].into(),
        open_seq: [(7, 2)].into(),
    };
    let before = index.search(&query("alpha")).unwrap();
    let bytes = index.snapshot(&wm).unwrap();
    let (restored, restored_wm) = Index::from_snapshot(&bytes).unwrap();
    assert_eq!(restored_wm, wm);
    assert_eq!(restored.search(&query("alpha")).unwrap(), before);
    assert_eq!(restored.indexed_hours(), [7].into());
}

#[test]
fn restored_index_accepts_writes_and_deletes() {
    let mut index = index_with(&[(r(7, 0), screen(now_ms(), "alpha"))]);
    let bytes = index.snapshot(&Watermark::default()).unwrap();
    let (mut restored, _) = Index::from_snapshot(&bytes).unwrap();
    restored.add(r(7, 1), &screen(now_ms(), "alpha")).unwrap();
    restored.commit().unwrap();
    assert_eq!(restored.search(&query("alpha")).unwrap().len(), 2);
    restored.delete_ref(r(7, 0)).unwrap();
    assert_eq!(refs(&restored, &query("alpha")), vec![r(7, 1)]);
    let second = restored.snapshot(&Watermark::default()).unwrap();
    let (again, _) = Index::from_snapshot(&second).unwrap();
    assert_eq!(refs(&again, &query("alpha")), vec![r(7, 1)]);
}

#[test]
fn garbage_snapshot_is_rejected() {
    assert!(Index::from_snapshot(&[0xff; 64]).is_err());
    assert!(Index::from_snapshot(&[]).is_err());
}

#[test]
fn delete_hour_removes_docs() {
    let mut index = index_with(&[
        (r(1, 0), screen(now_ms(), "alpha")),
        (r(2, 0), screen(now_ms(), "alpha")),
    ]);
    index.delete_hour(1).unwrap();
    assert_eq!(refs(&index, &query("alpha")), vec![r(2, 0)]);
    assert_eq!(index.indexed_hours(), [2].into());
}

#[test]
fn delete_app_returns_refs() {
    let mut index = index_with(&[
        (r(1, 0), rec(now_ms(), Kind::Screen, "Slack", "1", "alpha")),
        (r(1, 1), rec(now_ms(), Kind::Screen, "kitty", "1", "alpha")),
        (
            r(2, 0),
            rec(now_ms(), Kind::Clipboard, "slack", "1", "alpha"),
        ),
    ]);
    let mut removed = index.delete_app("slack").unwrap();
    removed.sort();
    assert_eq!(removed, vec![r(1, 0), r(2, 0)]);
    assert_eq!(refs(&index, &query("alpha")), vec![r(1, 1)]);
}

#[test]
fn delete_range_and_kind_before() {
    let now = now_ms();
    let mut index = index_with(&[
        (r(1, 0), rec(now - 5000, Kind::Clipboard, "a", "1", "x")),
        (r(1, 1), rec(now - 3000, Kind::Clipboard, "a", "1", "x")),
        (r(1, 2), rec(now - 3000, Kind::Screen, "a", "1", "x")),
        (r(1, 3), rec(now - 1000, Kind::Screen, "a", "1", "x")),
    ]);
    let old_clips = index
        .delete_kind_before(Kind::Clipboard, now - 4000)
        .unwrap();
    assert_eq!(old_clips, vec![r(1, 0)]);
    let ranged = index.delete_range(now - 3500, now - 2000).unwrap();
    assert_eq!(ranged.len(), 2);
    assert_eq!(refs(&index, &query("")), vec![r(1, 3)]);
}

#[test]
fn timeline_ordered() {
    let now = now_ms();
    let index = index_with(&[
        (r(1, 0), rec(now - 1000, Kind::Screen, "kitty", "1", "c")),
        (
            r(1, 1),
            rec(now - 3000, Kind::Clipboard, "Firefox", "1", "a"),
        ),
        (r(1, 2), rec(now - 2000, Kind::Screen, "foot", "1", "b")),
        (
            r(1, 3),
            rec(now - 90_000, Kind::Screen, "foot", "1", "out of range"),
        ),
    ]);
    let entries = index.timeline(now - 5000, now).unwrap();
    let order: Vec<_> = entries.iter().map(|e| e.r).collect();
    assert_eq!(order, vec![r(1, 1), r(1, 2), r(1, 0)]);
    assert_eq!(entries[0].kind, Kind::Clipboard);
    assert_eq!(entries[0].app, "Firefox");
    assert_eq!(entries[0].ts, now - 3000);
}

#[test]
fn empty_query_recent_first() {
    let now = now_ms();
    let index = index_with(&[
        (r(1, 0), screen(now - 2000, "old")),
        (r(1, 1), screen(now, "new")),
        (r(1, 2), screen(now - 1000, "mid")),
    ]);
    assert_eq!(refs(&index, &query("")), vec![r(1, 1), r(1, 2), r(1, 0)]);
    let mut limited = query("");
    limited.limit = 2;
    assert_eq!(refs(&index, &limited), vec![r(1, 1), r(1, 2)]);
}

#[test]
fn detail_returns_cwd_url_and_context() {
    let mut terminal = screen(now_ms(), "cargo build output");
    terminal.window.cwd = Some("/home/u/proj".into());
    let mut browser = rec(now_ms(), Kind::Screen, "firefox", "2", "docs page");
    browser.url = Some("https://example.org/docs".into());
    let index = index_with(&[(r(1, 0), terminal), (r(1, 1), browser)]);

    let detail = index.detail(r(1, 0)).unwrap().unwrap();
    assert_eq!(detail.cwd.as_deref(), Some("/home/u/proj"));
    assert_eq!(detail.hit.context.as_deref(), Some("/home/u/proj"));
    assert_eq!(detail.text, "cargo build output");

    let detail = index.detail(r(1, 1)).unwrap().unwrap();
    assert_eq!(detail.url.as_deref(), Some("https://example.org/docs"));
    assert_eq!(
        detail.hit.context.as_deref(),
        Some("https://example.org/docs")
    );
    assert_eq!(detail.hit.app, "firefox");
    assert_eq!(detail.hit.workspace, "2");

    assert!(index.detail(r(9, 9)).unwrap().is_none());
}

#[test]
fn uncommitted_adds_are_invisible_until_commit() {
    let mut index = Index::new().unwrap();
    index.add(r(1, 0), &screen(now_ms(), "alpha")).unwrap();
    assert!(refs(&index, &query("alpha")).is_empty());
    index.commit().unwrap();
    assert_eq!(refs(&index, &query("alpha")).len(), 1);
}

#[test]
fn url_and_cwd_are_searchable() {
    let mut record = screen(now_ms(), "page");
    record.url = Some("https://example.org/docs".into());
    record.window.cwd = Some("/home/u/capricorn".into());
    let index = index_with(&[(r(1, 0), record)]);
    assert_eq!(refs(&index, &query("example")), vec![r(1, 0)]);
    assert_eq!(refs(&index, &query("capricorn")), vec![r(1, 0)]);
}

#[test]
#[ignore = "latency measurement"]
fn search_latency_10k_docs() {
    let now = now_ms();
    let words = [
        "flake", "nixpkgs", "cargo", "build", "error", "terminal", "browser", "docs", "kernel",
        "hyprland", "config", "module", "rust", "tantivy", "search", "index",
    ];
    let mut index = Index::new().unwrap();
    for i in 0..10_000u32 {
        let text: Vec<_> = (0..60)
            .map(|j| words[((i * 7 + j * 13) % 16) as usize])
            .collect();
        index
            .add(
                r(i / 100, i % 100),
                &screen(now - i64::from(i) * 1000, &text.join(" ")),
            )
            .unwrap();
    }
    index.commit().unwrap();
    for text in ["flake", "flkae", "nixp", "cargo build error", "tantivy"] {
        let start = std::time::Instant::now();
        let hits = index.search(&query(text)).unwrap();
        eprintln!("{text:?}: {} hits in {:?}", hits.len(), start.elapsed());
    }
    let start = std::time::Instant::now();
    let bytes = index.snapshot(&Watermark::default()).unwrap();
    eprintln!("snapshot {} bytes in {:?}", bytes.len(), start.elapsed());
}
