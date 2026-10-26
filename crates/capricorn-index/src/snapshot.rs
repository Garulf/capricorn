use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tantivy::directory::RamDirectory;
use tantivy::index::SegmentComponent;
use tantivy::{Directory, Index as TantivyIndex};
use zeroize::Zeroizing;

use crate::schema;

const FORMAT_VERSION: u8 = 1;
const META_FILE: &str = "meta.json";
const MANAGED_FILE: &str = ".managed.json";
const CONSISTENT_READ_ATTEMPTS: usize = 5;
const SEGMENT_COMPONENTS: [SegmentComponent; 7] = [
    SegmentComponent::Postings,
    SegmentComponent::Positions,
    SegmentComponent::Terms,
    SegmentComponent::Store,
    SegmentComponent::FastFields,
    SegmentComponent::FieldNorms,
    SegmentComponent::Delete,
];

/// How far the index has caught up with the vault, so the daemon only ingests what is missing.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Watermark {
    pub sealed_hours: BTreeSet<u32>,
    pub open_seq: BTreeMap<u32, u32>,
}

#[derive(Serialize)]
struct SnapshotOut<'a> {
    version: u8,
    watermark: &'a Watermark,
    files: Vec<(String, &'a [u8])>,
}

#[derive(Deserialize)]
struct SnapshotIn {
    version: u8,
    watermark: Watermark,
    files: Vec<(String, Zeroizing<Vec<u8>>)>,
}

pub(crate) fn serialize(
    index: &TantivyIndex,
    directory: &RamDirectory,
    watermark: &Watermark,
) -> Result<Zeroizing<Vec<u8>>> {
    for _ in 0..CONSISTENT_READ_ATTEMPTS {
        if let Some(bytes) = try_serialize(index, directory, watermark)? {
            return Ok(bytes);
        }
    }
    bail!("index kept changing while taking a snapshot")
}

/// `None` when a background merge replaced the meta file mid-read.
fn try_serialize(
    index: &TantivyIndex,
    directory: &RamDirectory,
    watermark: &Watermark,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let meta_before = directory.atomic_read(Path::new(META_FILE))?;
    let Some(contents) = read_live_files(index, directory)? else {
        return Ok(None);
    };
    if directory.atomic_read(Path::new(META_FILE))? != meta_before {
        return Ok(None);
    }
    let files: Vec<(String, &[u8])> = contents
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.as_slice()))
        .collect();
    let capacity = files
        .iter()
        .map(|(name, bytes)| name.len() + bytes.len() + 16)
        .sum::<usize>()
        + 64;
    let out = SnapshotOut {
        version: FORMAT_VERSION,
        watermark,
        files,
    };
    let bytes = postcard::to_extend(&out, Vec::with_capacity(capacity))?;
    Ok(Some(Zeroizing::new(bytes)))
}

fn read_live_files(
    index: &TantivyIndex,
    directory: &RamDirectory,
) -> Result<Option<Vec<(String, tantivy::directory::OwnedBytes)>>> {
    let mut contents = Vec::new();
    for path in live_files(index, directory)? {
        let Ok(slice) = directory.open_read(&path) else {
            return Ok(None);
        };
        contents.push((path.to_string_lossy().into_owned(), slice.read_bytes()?));
    }
    Ok(Some(contents))
}

fn live_files(index: &TantivyIndex, directory: &RamDirectory) -> Result<Vec<PathBuf>> {
    let mut paths = vec![PathBuf::from(META_FILE)];
    let managed = PathBuf::from(MANAGED_FILE);
    if directory.exists(&managed)? {
        paths.push(managed);
    }
    for meta in index.searchable_segment_metas()? {
        for component in SEGMENT_COMPONENTS {
            let path = meta.relative_path(component);
            if directory.exists(&path)? {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

pub(crate) fn deserialize(
    bytes: &[u8],
) -> Result<(TantivyIndex, RamDirectory, schema::Fields, Watermark)> {
    let snapshot: SnapshotIn = postcard::from_bytes(bytes).context("malformed index snapshot")?;
    if snapshot.version != FORMAT_VERSION {
        bail!("unsupported index snapshot version {}", snapshot.version);
    }
    let directory = RamDirectory::create();
    for (name, data) in &snapshot.files {
        directory.atomic_write(Path::new(name), data)?;
    }
    let index = TantivyIndex::open(directory.clone())?;
    let (expected_schema, fields) = schema::build();
    if index.schema() != expected_schema {
        bail!("index snapshot has an unexpected schema");
    }
    schema::register_tokenizers(&index);
    Ok((index, directory, fields, snapshot.watermark))
}
