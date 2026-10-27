//! Sealed segment file: `b"CAPSEG-S1"`, u32 BE length, sealed file key, u64 BE
//! body_plain_len, then a [`crate::stream`] body. The body plaintext is the records back
//! to back, then postcard `Vec<(seq, offset, len)>`, then the table offset as u64 BE.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use zeroize::Zeroizing;

use super::format::{
    HourKey, commit_temp, corrupt, key_header_len, new_temp, random_key, read_key_header, read_u64,
    seal_key, sealed_path, unseal_key, write_key_header,
};
use crate::keyfile::{Identity, Recipient, Result};
use crate::stream::{StreamEncryptor, read_range, sealed_len};

const MAGIC: &[u8] = b"CAPSEG-S1";
const TABLE_OFFSET_LEN: u64 = 8;

/// Streams records into a `<hour>.*.tmp` file under a fresh file key; `commit` renames it over
/// `<hour>.seg`. Dropping it uncommitted deletes the temp file.
pub(super) struct SealedWriter {
    out: StreamEncryptor<BufWriter<NamedTempFile>>,
    plain_len_at: u64,
    table: Vec<(u32, u64, u64)>,
    position: u64,
    dest: PathBuf,
    hour: u32,
}

impl SealedWriter {
    pub fn create(dir: &Path, hour: u32, recipient: &Recipient) -> Result<Self> {
        let key = random_key();
        let sealed_key = seal_key(recipient, &key, hour)?;
        let mut temp = new_temp(dir, hour)?;
        let plain_len_at = write_key_header(&mut temp, MAGIC, &sealed_key)?;
        temp.write_all(&0u64.to_be_bytes())?;
        let out = StreamEncryptor::new(&key, BufWriter::new(temp))?;
        Ok(Self {
            out,
            plain_len_at,
            table: Vec::new(),
            position: 0,
            dest: sealed_path(dir, hour),
            hour,
        })
    }

    pub fn add(&mut self, seq: u32, record: &[u8]) -> Result<()> {
        let len = record.len() as u64;
        self.table.push((seq, self.position, len));
        self.out.write_all(record)?;
        self.position += len;
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        let table = Zeroizing::new(postcard::to_stdvec(&self.table).map_err(corrupt)?);
        let table_offset = self.position;
        self.out.write_all(&table)?;
        self.out.write_all(&table_offset.to_be_bytes())?;
        let plain_len = table_offset + table.len() as u64 + TABLE_OFFSET_LEN;
        let mut temp = self
            .out
            .finish()?
            .into_inner()
            .map_err(|e| e.into_error())?;
        temp.seek(SeekFrom::Start(self.plain_len_at))?;
        temp.write_all(&plain_len.to_be_bytes())?;
        commit_temp(temp, &self.dest, self.hour)
    }
}

/// Where each record sits in the body plaintext. Holds no record content.
#[derive(Debug, Default)]
pub(super) struct SealedTable {
    entries: BTreeMap<u32, (u64, u64)>,
}

impl SealedTable {
    pub fn get(&self, seq: u32) -> Option<(u64, u64)> {
        self.entries.get(&seq).copied()
    }

    pub fn seqs(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.keys().copied()
    }

    pub fn contains_any(&self, seqs: &[u32]) -> bool {
        seqs.iter().any(|seq| self.entries.contains_key(seq))
    }
}

pub(super) struct SealedFile {
    file: File,
    key: HourKey,
    body_start: u64,
    body_plain_len: u64,
}

impl SealedFile {
    pub fn open(path: &Path, hour: u32, identity: &Identity) -> Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let sealed_key = read_key_header(&mut reader, MAGIC)?;
        let body_plain_len =
            read_u64(&mut reader).map_err(|_| corrupt("segment header truncated"))?;
        let body_start = key_header_len(MAGIC, &sealed_key) + 8;
        let expected_len = sealed_len(body_plain_len).and_then(|len| len.checked_add(body_start));
        if body_plain_len < TABLE_OFFSET_LEN
            || body_plain_len > file_len
            || expected_len != Some(file_len)
        {
            return Err(corrupt("sealed segment length mismatch"));
        }
        Ok(Self {
            file: reader.into_inner(),
            key: unseal_key(identity, &sealed_key, hour)?,
            body_start,
            body_plain_len,
        })
    }

    pub fn read_table(&mut self) -> Result<SealedTable> {
        let offset_at = self.body_plain_len - TABLE_OFFSET_LEN;
        let table_offset = u64::from_be_bytes(
            self.read_plain(offset_at, TABLE_OFFSET_LEN)?[..]
                .try_into()
                .map_err(|_| corrupt("table offset"))?,
        );
        if table_offset > offset_at {
            return Err(corrupt("table offset past end"));
        }
        let bytes = self.read_plain(table_offset, offset_at - table_offset)?;
        let rows: Vec<(u32, u64, u64)> = postcard::from_bytes(&bytes).map_err(corrupt)?;
        let mut entries = BTreeMap::new();
        for (seq, offset, len) in rows {
            let fits = offset
                .checked_add(len)
                .is_some_and(|end| end <= table_offset);
            if !fits || entries.insert(seq, (offset, len)).is_some() {
                return Err(corrupt("bad segment table"));
            }
        }
        Ok(SealedTable { entries })
    }

    pub fn read_plain(&mut self, offset: u64, len: u64) -> Result<Zeroizing<Vec<u8>>> {
        read_range(
            &self.key,
            &mut self.file,
            self.body_start,
            self.body_plain_len,
            offset,
            len,
        )
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidData => corrupt("sealed segment failed authentication"),
            _ => e.into(),
        })
    }
}
