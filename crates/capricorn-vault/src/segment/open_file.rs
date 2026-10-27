//! Open segment file: `b"CAPSEG-O1"`, u32 BE length, sealed hour key, then frames
//! `[u32 BE ciphertext len][u32 BE seq][24 B nonce][ciphertext]`. Each frame is
//! XChaCha20-Poly1305 under the hour key with AAD `hour BE || seq BE`. The seq is in the
//! clear so frames can be counted, deleted and copied without the key. Until the hour is
//! sealed, the file reveals each record's ciphertext size and the record count.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use super::format::{
    HourKey, commit_temp, corrupt, key_header_len, new_temp, random_key, read_key_header, read_u32,
    seal_key, stamp_hour, unseal_key, write_key_header,
};
use crate::keyfile::{Identity, Recipient, Result};

const MAGIC: &[u8] = b"CAPSEG-O1";
const NONCE_LEN: usize = 24;
const TAG_LEN: u32 = 16;
const FRAME_HEADER_LEN: u64 = 4 + 4 + NONCE_LEN as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Frame {
    pub seq: u32,
    pub offset: u64,
    pub len: u64,
}

impl Frame {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

pub(super) struct Scan {
    pub sealed_key: Vec<u8>,
    pub header_len: u64,
    /// Complete frames only: a final frame running past EOF is a torn write and is left out.
    pub frames: Vec<Frame>,
}

impl Scan {
    pub fn unseal(&self, identity: &Identity, hour: u32) -> Result<HourKey> {
        unseal_key(identity, &self.sealed_key, hour)
    }

    pub fn find(&self, seq: u32) -> Option<Frame> {
        self.frames.iter().copied().find(|frame| frame.seq == seq)
    }
}

/// Creates an empty open file atomically and returns its fresh key and header length.
pub(super) fn create(path: &Path, hour: u32, recipient: &Recipient) -> Result<(HourKey, u64)> {
    let key = random_key();
    let sealed_key = seal_key(recipient, &key, hour)?;
    let mut temp = new_temp(parent(path), hour)?;
    let header_len = write_key_header(&mut temp, MAGIC, &sealed_key)?;
    commit_temp(temp, path, hour)?;
    Ok((key, header_len))
}

pub(super) fn scan(path: &Path) -> Result<Scan> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let sealed_key = read_key_header(&mut reader, MAGIC)?;
    let header_len = key_header_len(MAGIC, &sealed_key);
    let mut frames = Vec::new();
    let mut offset = header_len;
    while offset + FRAME_HEADER_LEN <= file_len {
        let ciphertext_len = read_u32(&mut reader)?;
        let seq = read_u32(&mut reader)?;
        let len = FRAME_HEADER_LEN + u64::from(ciphertext_len);
        if ciphertext_len < TAG_LEN || offset + len > file_len {
            break;
        }
        frames.push(Frame { seq, offset, len });
        reader.seek_relative(len as i64 - 8)?;
        offset += len;
    }
    Ok(Scan {
        sealed_key,
        header_len,
        frames,
    })
}

pub(super) fn seal_frame(key: &HourKey, hour: u32, seq: u32, plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rand::fill(&mut nonce);
    let ciphertext = cipher(key)
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &aad(hour, seq),
            },
        )
        .map_err(|_| corrupt("record encryption failed"))?;
    let ciphertext_len =
        u32::try_from(ciphertext.len()).map_err(|_| corrupt("record too large"))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN as usize + ciphertext.len());
    frame.extend_from_slice(&ciphertext_len.to_be_bytes());
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&nonce);
    frame.extend_from_slice(&ciphertext);
    Ok(frame)
}

/// Appends at `end`, first cutting off anything a failed earlier append left behind, and
/// resets the file times to the hour start.
pub(super) fn append_frame(path: &Path, hour: u32, end: u64, frame: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    if file.metadata()?.len() != end {
        file.set_len(end)?;
    }
    file.seek(SeekFrom::Start(end))?;
    let written = file
        .write_all(frame)
        .and_then(|()| stamp_hour(&file, hour))
        .and_then(|()| file.sync_data());
    if written.is_err() {
        let _ = file.set_len(end);
    }
    Ok(written?)
}

/// Decrypts one frame. `None` means it failed authentication.
pub(super) fn read_record(
    path: &Path,
    key: &HourKey,
    hour: u32,
    frame: Frame,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let mut file = File::open(path)?;
    let bytes = read_frame_bytes(&mut file, frame)?;
    Ok(open_frame(key, hour, frame.seq, &bytes))
}

/// Calls `visit` with every record. A final frame that fails authentication is a torn
/// write and is skipped when `torn_tail_allowed`; any other failure is an error.
pub(super) fn for_each_record(
    path: &Path,
    key: &HourKey,
    hour: u32,
    frames: &[Frame],
    torn_tail_allowed: bool,
    mut visit: impl FnMut(u32, Zeroizing<Vec<u8>>) -> Result<()>,
) -> Result<()> {
    let mut file = File::open(path)?;
    for (index, &frame) in frames.iter().enumerate() {
        let bytes = read_frame_bytes(&mut file, frame)?;
        match open_frame(key, hour, frame.seq, &bytes) {
            Some(plaintext) => visit(frame.seq, plaintext)?,
            None if torn_tail_allowed && index + 1 == frames.len() => {}
            None => return Err(corrupt("segment record failed authentication")),
        }
    }
    Ok(())
}

/// Atomically rewrites the file with only the frames `keep` accepts, copying their
/// ciphertext verbatim. Returns the kept frames at their new offsets.
pub(super) fn rewrite_keeping(
    path: &Path,
    hour: u32,
    header_len: u64,
    frames: &[Frame],
    keep: impl Fn(&Frame) -> bool,
) -> Result<Vec<Frame>> {
    let mut source = File::open(path)?;
    let mut temp = new_temp(parent(path), hour)?;
    let mut header = vec![0u8; header_len as usize];
    source.read_exact(&mut header)?;
    temp.write_all(&header)?;
    let mut kept = Vec::new();
    let mut offset = header_len;
    for &frame in frames.iter().filter(|frame| keep(frame)) {
        temp.write_all(&read_frame_bytes(&mut source, frame)?)?;
        kept.push(Frame { offset, ..frame });
        offset += frame.len;
    }
    commit_temp(temp, path, hour)?;
    Ok(kept)
}

fn read_frame_bytes(file: &mut File, frame: Frame) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; frame.len as usize];
    file.seek(SeekFrom::Start(frame.offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn open_frame(key: &HourKey, hour: u32, seq: u32, bytes: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let header = FRAME_HEADER_LEN as usize;
    let stored_seq = u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?);
    if stored_seq != seq {
        return None;
    }
    let nonce: [u8; NONCE_LEN] = bytes.get(8..header)?.try_into().ok()?;
    cipher(key)
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: bytes.get(header..)?,
                aad: &aad(hour, seq),
            },
        )
        .ok()
        .map(Zeroizing::new)
}

fn aad(hour: u32, seq: u32) -> [u8; 8] {
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(&hour.to_be_bytes());
    aad[4..].copy_from_slice(&seq.to_be_bytes());
    aad
}

fn cipher(key: &HourKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(&key[..]).expect("hour key is 32 bytes")
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}
