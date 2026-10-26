//! Chunked XChaCha20-Poly1305 encryption with random access.
//!
//! Layout: a 16-byte random nonce prefix, then chunks of [`CHUNK_LEN`] plaintext bytes,
//! each followed by its 16-byte tag. Chunk `i` uses nonce `prefix || i as u64 BE` and
//! AAD `[is_last]`, so chunks cannot be reordered, dropped or appended undetected.
//! An empty plaintext still produces one (empty, final) chunk.

use std::io::{self, Read, Seek, SeekFrom, Write};

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

pub const CHUNK_LEN: usize = 64 * 1024;
pub const PREFIX_LEN: usize = 16;
pub const TAG_LEN: usize = 16;

const SEALED_CHUNK_LEN: u64 = (CHUNK_LEN + TAG_LEN) as u64;

/// Encrypts `plaintext` under `key` and writes the whole stream to `out`.
pub fn encrypt_stream(key: &[u8; 32], plaintext: &[u8], out: &mut impl Write) -> io::Result<()> {
    let mut encryptor = StreamEncryptor::new(key, out)?;
    encryptor.write_all(plaintext)?;
    encryptor.finish()?;
    Ok(())
}

/// Returns `len` plaintext bytes starting at `offset`, decrypting only the chunks that
/// cover the range. `body_start` is where the stream (its nonce prefix) begins in `file`.
pub fn read_range(
    key: &[u8; 32],
    file: &mut (impl Read + Seek),
    body_start: u64,
    body_plain_len: u64,
    offset: u64,
    len: u64,
) -> io::Result<Zeroizing<Vec<u8>>> {
    let end = offset
        .checked_add(len)
        .filter(|&end| end <= body_plain_len)
        .ok_or_else(|| invalid_input("range past end of stream"))?;
    let mut out = Zeroizing::new(Vec::with_capacity(usize_from(len)?));
    if len == 0 {
        return Ok(out);
    }
    let layout = Layout::new(body_plain_len);
    let prefix = read_prefix(file, body_start)?;
    let cipher = cipher(key);
    let chunk_len = CHUNK_LEN as u64;
    for index in offset / chunk_len..=(end - 1) / chunk_len {
        let chunk = layout.decrypt_chunk(&cipher, &prefix, file, body_start, index)?;
        let chunk_start = index * chunk_len;
        let from = usize_from(offset.saturating_sub(chunk_start))?;
        let to = usize_from((end - chunk_start).min(chunk.len() as u64))?;
        out.extend_from_slice(&chunk[from..to]);
    }
    Ok(out)
}

/// Incremental form of [`encrypt_stream`] for plaintext too large to hold at once.
pub struct StreamEncryptor<W: Write> {
    cipher: XChaCha20Poly1305,
    prefix: [u8; PREFIX_LEN],
    next_index: u64,
    pending: Zeroizing<Vec<u8>>,
    out: W,
}

impl<W: Write> StreamEncryptor<W> {
    /// Writes a fresh random nonce prefix to `out`.
    pub fn new(key: &[u8; 32], mut out: W) -> io::Result<Self> {
        let mut prefix = [0u8; PREFIX_LEN];
        rand::fill(&mut prefix);
        out.write_all(&prefix)?;
        Ok(Self {
            cipher: cipher(key),
            prefix,
            next_index: 0,
            pending: Zeroizing::new(Vec::with_capacity(2 * CHUNK_LEN)),
            out,
        })
    }

    pub fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let room = 2 * CHUNK_LEN - self.pending.len();
            let (now, rest) = data.split_at(room.min(data.len()));
            self.pending.extend_from_slice(now);
            data = rest;
            self.flush_full_chunks()?;
        }
        Ok(())
    }

    /// Emits the final chunk and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        let last = std::mem::take(&mut *self.pending);
        self.emit(&Zeroizing::new(last), true)?;
        Ok(self.out)
    }

    /// Keeps at least one chunk pending, because only `finish` knows which chunk is last.
    fn flush_full_chunks(&mut self) -> io::Result<()> {
        while self.pending.len() > CHUNK_LEN {
            let chunk = Zeroizing::new(self.pending[..CHUNK_LEN].to_vec());
            self.pending.drain(..CHUNK_LEN);
            self.emit(&chunk, false)?;
        }
        Ok(())
    }

    fn emit(&mut self, chunk: &[u8], is_last: bool) -> io::Result<()> {
        let sealed = self
            .cipher
            .encrypt(
                &chunk_nonce(&self.prefix, self.next_index),
                Payload {
                    msg: chunk,
                    aad: &[u8::from(is_last)],
                },
            )
            .map_err(|_| io::Error::other("chunk encryption failed"))?;
        self.out.write_all(&sealed)?;
        self.next_index += 1;
        Ok(())
    }
}

/// Total on-disk length of a stream holding `plain_len` bytes.
pub fn sealed_len(plain_len: u64) -> u64 {
    let layout = Layout::new(plain_len);
    PREFIX_LEN as u64 + layout.chunk_count * TAG_LEN as u64 + plain_len
}

struct Layout {
    plain_len: u64,
    chunk_count: u64,
}

impl Layout {
    fn new(plain_len: u64) -> Self {
        Self {
            plain_len,
            chunk_count: plain_len.div_ceil(CHUNK_LEN as u64).max(1),
        }
    }

    fn plain_len_of(&self, index: u64) -> u64 {
        let chunk_len = CHUNK_LEN as u64;
        if index + 1 == self.chunk_count {
            self.plain_len - index * chunk_len
        } else {
            chunk_len
        }
    }

    fn decrypt_chunk(
        &self,
        cipher: &XChaCha20Poly1305,
        prefix: &[u8; PREFIX_LEN],
        file: &mut (impl Read + Seek),
        body_start: u64,
        index: u64,
    ) -> io::Result<Zeroizing<Vec<u8>>> {
        let position = body_start + PREFIX_LEN as u64 + index * SEALED_CHUNK_LEN;
        let mut sealed = vec![0u8; usize_from(self.plain_len_of(index))? + TAG_LEN];
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut sealed)?;
        let is_last = index + 1 == self.chunk_count;
        cipher
            .decrypt(
                &chunk_nonce(prefix, index),
                Payload {
                    msg: &sealed,
                    aad: &[u8::from(is_last)],
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk failed authentication"))
    }
}

fn read_prefix(file: &mut (impl Read + Seek), body_start: u64) -> io::Result<[u8; PREFIX_LEN]> {
    let mut prefix = [0u8; PREFIX_LEN];
    file.seek(SeekFrom::Start(body_start))?;
    file.read_exact(&mut prefix)?;
    Ok(prefix)
}

fn chunk_nonce(prefix: &[u8; PREFIX_LEN], index: u64) -> XNonce {
    let mut nonce = [0u8; 24];
    nonce[..PREFIX_LEN].copy_from_slice(prefix);
    nonce[PREFIX_LEN..].copy_from_slice(&index.to_be_bytes());
    XNonce::from(nonce)
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(key).expect("stream key is 32 bytes")
}

fn usize_from(value: u64) -> io::Result<usize> {
    usize::try_from(value).map_err(|_| invalid_input("length does not fit in memory"))
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use rand::Rng;

    use super::*;

    fn random_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        rand::fill(&mut key);
        key
    }

    fn random_bytes(len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        rand::fill(&mut bytes[..]);
        bytes
    }

    fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encrypt_stream(key, plaintext, &mut out).unwrap();
        out
    }

    fn read(
        key: &[u8; 32],
        sealed: &[u8],
        plain_len: u64,
        offset: u64,
        len: u64,
    ) -> io::Result<Vec<u8>> {
        read_range(key, &mut Cursor::new(sealed), 0, plain_len, offset, len).map(|b| b.to_vec())
    }

    #[test]
    fn stream_roundtrip_random_ranges() {
        let key = random_key();
        let plaintext = random_bytes(1024 * 1024);
        let sealed = encrypt(&key, &plaintext);
        assert_eq!(sealed.len() as u64, sealed_len(plaintext.len() as u64));
        let total = plaintext.len() as u64;
        let mut rng = rand::rng();
        for _ in 0..50 {
            let offset = rng.random_range(0..total);
            let len = rng.random_range(0..=(total - offset).min(200_000));
            let got = read(&key, &sealed, total, offset, len).unwrap();
            assert_eq!(got, &plaintext[offset as usize..(offset + len) as usize]);
        }

        let mut tampered = sealed.clone();
        tampered[PREFIX_LEN + 3 * (CHUNK_LEN + TAG_LEN) + 7] ^= 1;
        let start = 3 * CHUNK_LEN as u64;
        assert!(read(&key, &tampered, total, start, 10).is_err());
        assert!(read(&key, &tampered, total, 0, 10).is_ok());
    }

    #[test]
    fn empty_and_exact_chunk_streams_roundtrip() {
        let key = random_key();
        for len in [0, 1, CHUNK_LEN - 1, CHUNK_LEN, CHUNK_LEN + 1, 2 * CHUNK_LEN] {
            let plaintext = random_bytes(len);
            let sealed = encrypt(&key, &plaintext);
            assert_eq!(sealed.len() as u64, sealed_len(len as u64));
            let got = read(&key, &sealed, len as u64, 0, len as u64).unwrap();
            assert_eq!(got, plaintext);
        }
    }

    #[test]
    fn incremental_writes_match_one_shot_layout() {
        let key = random_key();
        let plaintext = random_bytes(3 * CHUNK_LEN + 17);
        let mut encryptor = StreamEncryptor::new(&key, Vec::new()).unwrap();
        for piece in plaintext.chunks(10_007) {
            encryptor.write_all(piece).unwrap();
        }
        let sealed = encryptor.finish().unwrap();
        let total = plaintext.len() as u64;
        assert_eq!(read(&key, &sealed, total, 0, total).unwrap(), plaintext);
    }

    #[test]
    fn truncating_whole_chunks_is_detected() {
        let key = random_key();
        let plaintext = random_bytes(3 * CHUNK_LEN);
        let sealed = encrypt(&key, &plaintext);
        let two_chunks = PREFIX_LEN + 2 * (CHUNK_LEN + TAG_LEN);
        let truncated = &sealed[..two_chunks];
        let shorter = 2 * CHUNK_LEN as u64;
        assert!(read(&key, truncated, shorter, shorter - 10, 10).is_err());
    }

    #[test]
    fn swapped_chunks_are_detected() {
        let key = random_key();
        let plaintext = random_bytes(3 * CHUNK_LEN);
        let mut sealed = encrypt(&key, &plaintext);
        let sealed_chunk = CHUNK_LEN + TAG_LEN;
        let (first, second) = (PREFIX_LEN, PREFIX_LEN + sealed_chunk);
        let copy = sealed[first..first + sealed_chunk].to_vec();
        sealed.copy_within(second..second + sealed_chunk, first);
        sealed[second..second + sealed_chunk].copy_from_slice(&copy);
        let total = plaintext.len() as u64;
        assert!(read(&key, &sealed, total, 0, 10).is_err());
        assert!(read(&key, &sealed, total, CHUNK_LEN as u64, 10).is_err());
    }

    #[test]
    fn wrong_key_and_out_of_range_fail() {
        let key = random_key();
        let sealed = encrypt(&key, b"hello world");
        assert!(read(&random_key(), &sealed, 11, 0, 5).is_err());
        assert!(read(&key, &sealed, 11, 8, 5).is_err());
        assert!(read(&key, &sealed, 11, u64::MAX, 5).is_err());
    }

    #[test]
    fn fresh_prefix_per_stream() {
        let key = random_key();
        let a = encrypt(&key, b"same");
        let b = encrypt(&key, b"same");
        assert_ne!(a[..PREFIX_LEN], b[..PREFIX_LEN]);
        assert_ne!(a, b);
    }
}
