//! The encrypted search index on disk.
//!
//! `index.key` holds a random 32-byte key sealed to the vault recipient. `index.snap`
//! holds the index snapshot encrypted with that key: an 8-byte big-endian plaintext
//! length followed by a seekable stream. Both files are replaced atomically.

use std::io::{Cursor, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use capricorn_index::{Index, Watermark};
use capricorn_vault::stream::{StreamEncryptor, read_range, sealed_len};
use capricorn_vault::{Identity, open_small, seal_small};
use zeroize::Zeroizing;

pub type IndexKey = Zeroizing<[u8; 32]>;

const KEY_FILE: &str = "index.key";
const SNAPSHOT_FILE: &str = "index.snap";
const LENGTH_PREFIX: u64 = 8;

pub struct IndexFiles {
    dir: PathBuf,
}

impl IndexFiles {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Opens `index.key`, creating a fresh key the first time.
    pub fn load_or_create_key(&self, id: &Identity) -> Result<IndexKey> {
        match std::fs::read(self.dir.join(KEY_FILE)) {
            Ok(sealed) => {
                let plain = open_small(id, &sealed).context("opening index key")?;
                let mut key = Zeroizing::new([0u8; 32]);
                if plain.len() != key.len() {
                    bail!("index key has the wrong length");
                }
                key.copy_from_slice(&plain);
                Ok(key)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.create_key(id),
            Err(e) => Err(e).context("reading index key"),
        }
    }

    /// Replaces the key and removes the snapshot it protected.
    pub fn create_key(&self, id: &Identity) -> Result<IndexKey> {
        let mut key = Zeroizing::new([0u8; 32]);
        rand::fill(&mut key[..]);
        let sealed = seal_small(&id.recipient(), &key[..])?;
        write_atomically(&self.dir, KEY_FILE, |out| Ok(out.write_all(&sealed)?))?;
        match std::fs::remove_file(self.dir.join(SNAPSHOT_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        Ok(key)
    }

    /// `None` when no snapshot has been written yet.
    pub fn load(&self, key: &IndexKey) -> Result<Option<(Index, Watermark)>> {
        let sealed = match std::fs::read(self.dir.join(SNAPSHOT_FILE)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading index snapshot"),
        };
        let plain = decrypt_snapshot(key, &sealed)?;
        Ok(Some(Index::from_snapshot(&plain)?))
    }

    pub fn save(&self, key: &IndexKey, index: &mut Index, watermark: &Watermark) -> Result<()> {
        let plain = index.snapshot(watermark)?;
        write_atomically(&self.dir, SNAPSHOT_FILE, |out| {
            out.write_all(&(plain.len() as u64).to_be_bytes())?;
            let mut encryptor = StreamEncryptor::new(key, out)?;
            encryptor.write_all(&plain)?;
            encryptor.finish()?;
            Ok(())
        })
    }
}

fn decrypt_snapshot(key: &IndexKey, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let prefix: [u8; 8] = sealed
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .context("index snapshot is truncated")?;
    let plain_len = u64::from_be_bytes(prefix);
    let actual = sealed.len() as u64;
    let expected = sealed_len(plain_len).and_then(|len| len.checked_add(LENGTH_PREFIX));
    if expected != Some(actual) {
        bail!("index snapshot has the wrong length");
    }
    let mut file = Cursor::new(sealed);
    read_range(key, &mut file, LENGTH_PREFIX, plain_len, 0, plain_len)
        .context("decrypting index snapshot")
}

/// Writes through a private temp file in `dir`, then renames it over `name`.
fn write_atomically(
    dir: &Path,
    name: &str,
    fill: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    fill(temp.as_file_mut())?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join(name)).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use capricorn_core::Record;
    use capricorn_core::proto::RecordRef;
    use capricorn_vault::{KdfParams, Vault};

    use super::*;

    fn identity(dir: &Path) -> Identity {
        let (_, id, _) = Vault::create(dir, "pw", KdfParams::insecure_fast(), false).unwrap();
        id
    }

    fn index_with(text: &str) -> Index {
        let mut index = Index::new().unwrap();
        let record = Record {
            ts: 1,
            text: text.into(),
            ..Record::default()
        };
        index.add(RecordRef { hour: 1, seq: 0 }, &record).unwrap();
        index.commit().unwrap();
        index
    }

    #[test]
    fn snapshot_roundtrips_and_is_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let id = identity(dir.path());
        let files = IndexFiles::new(dir.path());
        let key = files.load_or_create_key(&id).unwrap();
        let canary = ["snap", "canary", "word"].concat();
        let mut index = index_with(&canary);
        let watermark = Watermark {
            sealed_hours: [1].into(),
            ..Watermark::default()
        };
        files.save(&key, &mut index, &watermark).unwrap();

        let on_disk = std::fs::read(dir.path().join(SNAPSHOT_FILE)).unwrap();
        assert!(
            !on_disk
                .windows(canary.len())
                .any(|w| w == canary.as_bytes())
        );

        let again = files.load_or_create_key(&id).unwrap();
        assert_eq!(*again, *key);
        let (_, loaded_watermark) = files.load(&again).unwrap().unwrap();
        assert_eq!(loaded_watermark, watermark);
    }

    #[test]
    fn missing_snapshot_is_none_and_tampering_fails() {
        let dir = tempfile::tempdir().unwrap();
        let id = identity(dir.path());
        let files = IndexFiles::new(dir.path());
        let key = files.load_or_create_key(&id).unwrap();
        assert!(files.load(&key).unwrap().is_none());

        files
            .save(&key, &mut index_with("x"), &Watermark::default())
            .unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(files.load(&key).is_err());
        std::fs::write(&path, &bytes[..4]).unwrap();
        assert!(files.load(&key).is_err());
        let mut hostile = u64::MAX.to_be_bytes().to_vec();
        hostile.extend_from_slice(&bytes[8..]);
        std::fs::write(&path, &hostile).unwrap();
        assert!(files.load(&key).is_err());
    }
}
