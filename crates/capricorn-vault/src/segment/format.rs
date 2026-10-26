//! Pieces shared by the open and sealed segment formats: file names, sealed key
//! headers and durable file replacement.

use std::fs::{self, File, FileTimes};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use capricorn_core::proto::Ts;
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

use crate::keyfile::{Identity, Recipient, Result, VaultError, open_small, seal_small};

pub(super) type HourKey = Zeroizing<[u8; 32]>;

pub const HOUR_MS: i64 = 3_600_000;

const MAX_KEY_BLOB_LEN: u32 = 64 * 1024;
const SEALED_SUFFIX: &str = ".seg";
const OPEN_SUFFIX: &str = ".open";
const TEMP_SUFFIX: &str = ".tmp";
const QUARANTINE_SUFFIX: &str = ".bad";
const FLOOR_SUFFIX: &str = ".floor";

/// Hour bucket (unix seconds / 3600) of a millisecond timestamp.
pub fn hour_of(ts: Ts) -> u32 {
    let hour = ts.div_euclid(HOUR_MS).clamp(0, i64::from(u32::MAX));
    hour as u32
}

pub fn hour_start_ms(hour: u32) -> Ts {
    i64::from(hour) * HOUR_MS
}

pub fn hour_end_ms(hour: u32) -> Ts {
    hour_start_ms(hour) + HOUR_MS
}

pub(super) fn corrupt(reason: impl std::fmt::Display) -> VaultError {
    VaultError::Corrupt(reason.to_string())
}

pub(super) fn random_key() -> HourKey {
    let mut key = Zeroizing::new([0u8; 32]);
    rand::fill(&mut key[..]);
    key
}

/// The key is sealed together with its hour so a file renamed to another hour fails to open.
pub(super) fn seal_key(recipient: &Recipient, key: &HourKey, hour: u32) -> Result<Vec<u8>> {
    let mut blob = Zeroizing::new([0u8; 36]);
    blob[..32].copy_from_slice(&key[..]);
    blob[32..].copy_from_slice(&hour.to_be_bytes());
    seal_small(recipient, &blob[..])
}

pub(super) fn unseal_key(identity: &Identity, sealed: &[u8], hour: u32) -> Result<HourKey> {
    let blob = open_small(identity, sealed)?;
    if blob.len() != 36 || blob[32..] != hour.to_be_bytes() {
        return Err(corrupt("segment key does not belong to this hour"));
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&blob[..32]);
    Ok(key)
}

/// Writes `magic || u32 BE len || sealed_key` and returns the number of bytes written.
pub(super) fn write_key_header(
    out: &mut impl Write,
    magic: &[u8],
    sealed_key: &[u8],
) -> io::Result<u64> {
    let len =
        u32::try_from(sealed_key.len()).map_err(|_| io::Error::other("key blob too large"))?;
    out.write_all(magic)?;
    out.write_all(&len.to_be_bytes())?;
    out.write_all(sealed_key)?;
    Ok((magic.len() + 4 + sealed_key.len()) as u64)
}

/// Reads the header written by [`write_key_header`] and returns the sealed key.
pub(super) fn read_key_header(input: &mut impl Read, magic: &[u8]) -> Result<Vec<u8>> {
    let mut found = vec![0u8; magic.len()];
    input
        .read_exact(&mut found)
        .map_err(|_| corrupt("segment header truncated"))?;
    if found != magic {
        return Err(corrupt("not a segment file"));
    }
    let len = read_u32(input).map_err(|_| corrupt("segment header truncated"))?;
    if len > MAX_KEY_BLOB_LEN {
        return Err(corrupt("segment key header too large"));
    }
    let mut sealed_key = vec![0u8; len as usize];
    input
        .read_exact(&mut sealed_key)
        .map_err(|_| corrupt("segment header truncated"))?;
    Ok(sealed_key)
}

pub(super) fn key_header_len(magic: &[u8], sealed_key: &[u8]) -> u64 {
    (magic.len() + 4 + sealed_key.len()) as u64
}

pub(super) fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

pub(super) fn read_u64(input: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum FileKind {
    Sealed,
    Open { suffix: u32 },
}

#[derive(Debug, Clone)]
pub(super) struct SegFile {
    pub hour: u32,
    pub kind: FileKind,
    pub path: PathBuf,
    pub bytes: u64,
}

pub(super) fn sealed_path(dir: &Path, hour: u32) -> PathBuf {
    dir.join(format!("{hour}{SEALED_SUFFIX}"))
}

pub(super) fn open_path(dir: &Path, hour: u32, suffix: u32) -> PathBuf {
    match suffix {
        0 => dir.join(format!("{hour}{OPEN_SUFFIX}")),
        n => dir.join(format!("{hour}.{n}{OPEN_SUFFIX}")),
    }
}

fn parse_name(name: &str) -> Option<(u32, FileKind)> {
    if let Some(hour) = name.strip_suffix(SEALED_SUFFIX) {
        return Some((hour.parse().ok()?, FileKind::Sealed));
    }
    let stem = name.strip_suffix(OPEN_SUFFIX)?;
    let (hour, suffix) = match stem.split_once('.') {
        Some((hour, suffix)) => (hour, suffix.parse().ok().filter(|&n| n > 0)?),
        None => (stem, 0),
    };
    Some((hour.parse().ok()?, FileKind::Open { suffix }))
}

/// Every segment file in `dir`, ordered by hour then kind.
pub(super) fn list_files(dir: &Path) -> Result<Vec<SegFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some((hour, kind)) = entry.file_name().to_str().and_then(parse_name) else {
            continue;
        };
        files.push(SegFile {
            hour,
            kind,
            path: entry.path(),
            bytes: entry.metadata()?.len(),
        });
    }
    files.sort_by_key(|file| (file.hour, file.path.clone()));
    Ok(files)
}

pub(super) fn floor_path(dir: &Path, hour: u32) -> PathBuf {
    dir.join(format!("{hour}{FLOOR_SUFFIX}"))
}

/// The lowest seq `hour` may still hand out. Records removed from an unsealed hour take
/// their seqs with them, so this file remembers how far the hour got; without it a new
/// open file could reuse the seq of a forgotten record that an older index still names.
/// It reveals at most how many records the hour once had, which its `.open` file
/// already did.
pub(super) fn read_floor(dir: &Path, hour: u32) -> Result<u32> {
    match fs::read(floor_path(dir, hour)) {
        Ok(bytes) => <[u8; 4]>::try_from(bytes.as_slice())
            .map(u32::from_be_bytes)
            .map_err(|_| corrupt("seq floor file is malformed")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn raise_floor(dir: &Path, hour: u32, next: u32) -> Result<()> {
    if read_floor(dir, hour)? >= next {
        return Ok(());
    }
    let mut temp = new_temp(dir, hour)?;
    temp.write_all(&next.to_be_bytes())?;
    commit_temp(temp, &floor_path(dir, hour), hour)
}

/// Leftovers of rewrites interrupted by a crash. They only ever hold ciphertext.
pub(super) fn remove_stale_temps(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(TEMP_SUFFIX) {
            fs::remove_file(entry.path())?;
        }
    }
    sync_dir(dir)
}

pub(super) fn new_temp(dir: &Path, hour: u32) -> Result<NamedTempFile> {
    Ok(tempfile::Builder::new()
        .prefix(&format!("{hour}."))
        .suffix(TEMP_SUFFIX)
        .tempfile_in(dir)?)
}

/// Sets a segment file's mtime and atime to the start of its hour, so file times reveal
/// no more than the hour bucket the name already does. ctime cannot be set.
pub(super) fn stamp_hour(file: &File, hour: u32) -> io::Result<()> {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(hour) * 3600);
    file.set_times(FileTimes::new().set_modified(start).set_accessed(start))
}

/// Stamps and fsyncs the temp file, renames it over `dest`, then fsyncs the directory.
pub(super) fn commit_temp(temp: NamedTempFile, dest: &Path, hour: u32) -> Result<()> {
    stamp_hour(temp.as_file(), hour)?;
    temp.as_file().sync_all()?;
    temp.persist(dest).map_err(|e| e.error)?;
    sync_parent(dest)
}

pub(super) fn remove_durably(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Renames `path` to `<name>.bad`, out of the way of every segment operation, and
/// returns the new path. The file still holds only ciphertext.
pub(super) fn quarantine(path: &Path) -> Result<PathBuf> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(QUARANTINE_SUFFIX);
    let target = path.with_file_name(name);
    fs::rename(path, &target)?;
    sync_parent(&target)?;
    Ok(target)
}

/// Quarantined files of `hour`, so removing the hour removes them too.
pub(super) fn quarantined_files(dir: &Path, hour: u32) -> Result<Vec<PathBuf>> {
    let prefix = format!("{hour}.");
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(QUARANTINE_SUFFIX) {
            files.push(dir.join(&*name));
        }
    }
    Ok(files)
}

fn sync_parent(path: &Path) -> Result<()> {
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_segment_names() {
        assert_eq!(parse_name("12.seg"), Some((12, FileKind::Sealed)));
        assert_eq!(
            parse_name("12.open"),
            Some((12, FileKind::Open { suffix: 0 }))
        );
        assert_eq!(
            parse_name("12.3.open"),
            Some((12, FileKind::Open { suffix: 3 }))
        );
        assert_eq!(parse_name("12.0.open"), None);
        assert_eq!(parse_name("12.ab.tmp"), None);
        assert_eq!(parse_name("x.seg"), None);
        assert_eq!(parse_name("vault.json"), None);
        assert_eq!(parse_name("12.floor"), None);
    }

    #[test]
    fn open_path_roundtrips_through_parse() {
        let dir = Path::new("/d");
        for suffix in [0, 1, 7] {
            let name = open_path(dir, 5, suffix);
            let parsed = parse_name(name.file_name().unwrap().to_str().unwrap());
            assert_eq!(parsed, Some((5, FileKind::Open { suffix })));
        }
    }

    #[test]
    fn hour_math() {
        assert_eq!(hour_of(0), 0);
        assert_eq!(hour_of(-1), 0);
        assert_eq!(hour_of(HOUR_MS - 1), 0);
        assert_eq!(hour_of(HOUR_MS), 1);
        assert_eq!(hour_end_ms(1), 2 * HOUR_MS);
    }
}
