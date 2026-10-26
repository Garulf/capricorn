//! In-memory full-text index over captured records, snapshotted as plaintext bytes
//! (the daemon encrypts them). Nothing here touches the disk.

mod highlight;
mod query;
mod schema;
mod snapshot;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use capricorn_core::Record;
use capricorn_core::proto::{Hit, Kind, RecordDetail, RecordRef, SearchQuery, TimelineEntry, Ts};
use tantivy::collector::TopDocs;
use tantivy::directory::RamDirectory;
use tantivy::query::{EnableScoring, Query};
use tantivy::schema::{Field, Value};
use tantivy::{
    DocAddress, DocId, Index as TantivyIndex, IndexReader, IndexWriter, Order, ReloadPolicy, Score,
    SegmentReader, TantivyDocument, Term,
};
use zeroize::Zeroizing;

use highlight::Highlighter;
use schema::Fields;

pub use snapshot::Watermark;

const WRITER_THREADS: usize = 1;
const WRITER_HEAP_BYTES: usize = 32_000_000;
const RECENCY_BOOST: f64 = 0.5;
const RECENCY_HALF_LIFE_DAYS: f64 = 7.0;
const MS_PER_DAY: f64 = 86_400_000.0;

pub struct Index {
    index: TantivyIndex,
    directory: RamDirectory,
    writer: IndexWriter,
    reader: IndexReader,
    fields: Fields,
}

impl Index {
    pub fn new() -> Result<Self> {
        let (index, directory, fields) = schema::new_in_ram()?;
        Self::assemble(index, directory, fields)
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<(Self, Watermark)> {
        let (index, directory, fields, watermark) = snapshot::deserialize(bytes)?;
        Ok((Self::assemble(index, directory, fields)?, watermark))
    }

    fn assemble(index: TantivyIndex, directory: RamDirectory, fields: Fields) -> Result<Self> {
        let writer = index.writer_with_num_threads(WRITER_THREADS, WRITER_HEAP_BYTES)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            directory,
            writer,
            reader,
            fields,
        })
    }

    /// Commits pending changes first, so the snapshot contains everything added so far.
    pub fn snapshot(&mut self, wm: &Watermark) -> Result<Zeroizing<Vec<u8>>> {
        self.commit()?;
        snapshot::serialize(&self.index, &self.directory, wm)
    }

    /// Not searchable until `commit`.
    pub fn add(&mut self, r: RecordRef, rec: &Record) -> Result<()> {
        let f = &self.fields;
        let mut doc = TantivyDocument::new();
        doc.add_u64(f.record_ref, r.to_u64());
        doc.add_u64(f.hour, u64::from(r.hour));
        doc.add_i64(f.ts, rec.ts);
        doc.add_text(f.kind, query::kind_name(rec.kind));
        doc.add_text(f.app, &rec.window.class);
        doc.add_text(f.app_key, &rec.window.class);
        doc.add_text(f.app_raw, &rec.window.class);
        doc.add_text(f.title, &rec.window.title);
        doc.add_text(f.workspace, &rec.window.workspace);
        doc.add_text(f.body, &rec.text);
        if let Some(cwd) = &rec.window.cwd {
            doc.add_text(f.cwd, cwd);
        }
        if let Some(url) = &rec.url {
            doc.add_text(f.url, url);
        }
        self.writer.add_document(doc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn delete_ref(&mut self, r: RecordRef) -> Result<()> {
        self.delete_term(Term::from_field_u64(self.fields.record_ref, r.to_u64()))
    }

    pub fn delete_hour(&mut self, hour: u32) -> Result<()> {
        self.delete_term(Term::from_field_u64(self.fields.hour, u64::from(hour)))
    }

    /// Case-insensitive on the window class.
    pub fn delete_app(&mut self, class: &str) -> Result<Vec<RecordRef>> {
        self.delete_matching(query::app_query(&self.fields, class))
    }

    /// Both bounds inclusive.
    pub fn delete_range(&mut self, from: Ts, to: Ts) -> Result<Vec<RecordRef>> {
        let range = query::ts_range(&self.fields, Some(from), Some(to));
        self.delete_matching(Box::new(range))
    }

    pub fn delete_kind_before(&mut self, kind: Kind, before: Ts) -> Result<Vec<RecordRef>> {
        let older = query::ts_range(&self.fields, None, Some(before - 1));
        let matching = query::all_of(vec![query::kind_query(&self.fields, kind), Box::new(older)]);
        self.delete_matching(matching)
    }

    /// Relevance with a recency boost; with no text, newest first. Ranges are inclusive.
    pub fn search(&self, q: &SearchQuery) -> Result<Vec<Hit>> {
        if q.limit == 0 {
            return Ok(Vec::new());
        }
        let limit = q.limit as usize;
        let terms = query::tokenize(&self.body_analyzer()?, &q.text);
        let built = query::search_query(&self.fields, &terms, q);
        let searcher = self.reader.searcher();
        let addresses = if terms.is_empty() {
            let newest_first =
                TopDocs::with_limit(limit).order_by_fast_field::<i64>("ts", Order::Desc);
            searcher
                .search(&built, &newest_first)?
                .into_iter()
                .map(|(_, address)| address)
                .collect()
        } else {
            self.top_by_relevance(&searcher, &built, limit)?
        };
        let highlighter = Highlighter::new(self.body_analyzer()?, terms);
        addresses
            .into_iter()
            .map(|address| {
                let doc: TantivyDocument = searcher.doc(address)?;
                Ok(self.hit(&doc, &highlighter))
            })
            .collect()
    }

    /// Oldest first, both bounds inclusive.
    pub fn timeline(&self, from: Ts, to: Ts) -> Result<Vec<TimelineEntry>> {
        let range = query::ts_range(&self.fields, Some(from), Some(to));
        let mut entries = Vec::new();
        for (segment, docs) in self.matching_docs(&range)? {
            let fast = segment.fast_fields();
            let refs = fast.u64("ref")?;
            let stamps = fast.i64("ts")?;
            let (Some(kinds), Some(apps)) = (fast.str("kind")?, fast.str("app_raw")?) else {
                continue;
            };
            for doc in docs {
                let (Some(r), Some(ts)) = (refs.first(doc), stamps.first(doc)) else {
                    continue;
                };
                entries.push(TimelineEntry {
                    r: RecordRef::from_u64(r),
                    ts,
                    kind: query::parse_kind(&fast_string(&kinds, doc)?),
                    app: fast_string(&apps, doc)?,
                });
            }
        }
        entries.sort_by_key(|entry| (entry.ts, entry.r));
        Ok(entries)
    }

    pub fn detail(&self, r: RecordRef) -> Result<Option<RecordDetail>> {
        let by_ref = query::exact_u64(self.fields.record_ref, r.to_u64());
        let searcher = self.reader.searcher();
        let Some((_, address)) = searcher
            .search(&by_ref, &TopDocs::with_limit(1).order_by_score())?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(address)?;
        let plain = Highlighter::new(self.body_analyzer()?, Vec::new());
        Ok(Some(RecordDetail {
            hit: self.hit(&doc, &plain),
            text: stored_str(&doc, self.fields.body).to_string(),
            cwd: optional_str(&doc, self.fields.cwd),
            url: optional_str(&doc, self.fields.url),
        }))
    }

    /// Refs of every committed record in `hour`.
    pub fn refs_in_hour(&self, hour: u32) -> Result<Vec<RecordRef>> {
        self.refs_matching(query::exact_u64(self.fields.hour, u64::from(hour)).as_ref())
    }

    pub fn indexed_hours(&self) -> BTreeSet<u32> {
        self.collect_hours().unwrap_or_default()
    }

    fn collect_hours(&self) -> Result<BTreeSet<u32>> {
        let mut hours = BTreeSet::new();
        for segment in self.reader.searcher().segment_readers() {
            let column = segment.fast_fields().u64("hour")?;
            hours.extend(
                segment
                    .doc_ids_alive()
                    .filter_map(|doc| column.first(doc))
                    .map(|hour| hour as u32),
            );
        }
        Ok(hours)
    }

    fn delete_term(&mut self, term: Term) -> Result<()> {
        self.writer.delete_term(term);
        self.commit()
    }

    fn delete_matching(&mut self, matching: Box<dyn Query>) -> Result<Vec<RecordRef>> {
        self.commit()?;
        let refs = self.refs_matching(matching.as_ref())?;
        self.writer.delete_query(matching)?;
        self.commit()?;
        Ok(refs)
    }

    fn refs_matching(&self, matching: &dyn Query) -> Result<Vec<RecordRef>> {
        let mut refs = Vec::new();
        for (segment, docs) in self.matching_docs(matching)? {
            let column = segment.fast_fields().u64("ref")?;
            refs.extend(
                docs.into_iter()
                    .filter_map(|doc| column.first(doc))
                    .map(RecordRef::from_u64),
            );
        }
        Ok(refs)
    }

    fn matching_docs(&self, matching: &dyn Query) -> Result<Vec<(SegmentReader, Vec<DocId>)>> {
        let searcher = self.reader.searcher();
        let weight = matching.weight(EnableScoring::disabled_from_searcher(&searcher))?;
        let mut found = Vec::new();
        for segment in searcher.segment_readers() {
            let mut docs = Vec::new();
            weight.for_each_no_score(segment, &mut |batch| docs.extend_from_slice(batch))?;
            docs.retain(|&doc| !segment.is_deleted(doc));
            found.push((segment.clone(), docs));
        }
        Ok(found)
    }

    fn top_by_relevance(
        &self,
        searcher: &tantivy::Searcher,
        built: &dyn Query,
        limit: usize,
    ) -> Result<Vec<DocAddress>> {
        let now = now_ms();
        let boosted = TopDocs::with_limit(limit).tweak_score(move |segment: &SegmentReader| {
            let stamps = segment.fast_fields().i64("ts").ok();
            move |doc: DocId, score: Score| {
                let ts = stamps.as_ref().and_then(|column| column.first(doc));
                score * recency_factor(now, ts.unwrap_or(0))
            }
        });
        Ok(searcher
            .search(built, &boosted)?
            .into_iter()
            .map(|(_, address)| address)
            .collect())
    }

    fn body_analyzer(&self) -> Result<tantivy::tokenizer::TextAnalyzer> {
        self.index
            .tokenizer_for_field(self.fields.body)
            .context("body tokenizer")
    }

    fn hit(&self, doc: &TantivyDocument, highlighter: &Highlighter) -> Hit {
        let f = &self.fields;
        let body = stored_str(doc, f.body);
        let title = stored_str(doc, f.title);
        let snippet = highlighter
            .snippet(f.body, body)
            .or_else(|| highlighter.snippet(f.title, title))
            .unwrap_or_else(|| highlight::Snippet {
                text: highlight::preview(if body.is_empty() { title } else { body }),
                highlights: Vec::new(),
            });
        Hit {
            r: RecordRef::from_u64(
                doc.get_first(f.record_ref)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            ),
            ts: doc.get_first(f.ts).and_then(|v| v.as_i64()).unwrap_or(0),
            kind: query::parse_kind(stored_str(doc, f.kind)),
            app: stored_str(doc, f.app_raw).to_string(),
            title: title.to_string(),
            workspace: stored_str(doc, f.workspace).to_string(),
            context: optional_str(doc, f.cwd).or_else(|| optional_str(doc, f.url)),
            snippet: snippet.text,
            highlights: snippet.highlights,
        }
    }
}

fn recency_factor(now: Ts, ts: Ts) -> f32 {
    let age_days = (now - ts).max(0) as f64 / MS_PER_DAY;
    (1.0 + RECENCY_BOOST * (-age_days / RECENCY_HALF_LIFE_DAYS).exp()) as f32
}

fn now_ms() -> Ts {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as Ts)
}

fn stored_str(doc: &TantivyDocument, field: Field) -> &str {
    doc.get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

fn optional_str(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn fast_string(column: &tantivy::columnar::StrColumn, doc: DocId) -> Result<String> {
    let mut out = String::new();
    if let Some(ord) = column.ords().first(doc) {
        column.ord_to_str(ord, &mut out)?;
    }
    Ok(out)
}
