use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use age::x25519;
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

pub type Recipient = x25519::Recipient;

const KEYFILE_NAME: &str = "vault.json";
const KEYFILE_VERSION: u32 = 1;
const RECOVERY_INFO: &[u8] = b"capricorn recovery";
const RECOVERY_CODE_BYTES: usize = 20;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TUNE_START_M_KIB: u32 = 256 * 1024;
const TUNE_MAX_T: u32 = 10;
const MAX_M_KIB: u32 = 4 * 1024 * 1024;
const MAX_T: u32 = 64;
const MAX_P: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("wrong password")]
    BadPassword,
    #[error("password must not be empty")]
    EmptyPassword,
    #[error("a vault already exists in this directory")]
    AlreadyExists,
    #[error("identity does not belong to this vault")]
    IdentityMismatch,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt vault: {0}")]
    Corrupt(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("vault is locked")]
    Locked,
    #[error("record {0} not found")]
    NotFound(capricorn_core::proto::RecordRef),
}

pub type Result<T, E = VaultError> = std::result::Result<T, E>;

fn corrupt(reason: impl fmt::Display) -> VaultError {
    VaultError::Corrupt(reason.to_string())
}

fn crypto(reason: impl fmt::Display) -> VaultError {
    VaultError::Crypto(reason.to_string())
}

mod hex {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer, T: AsRef<[u8]>>(
        bytes: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded: String = bytes
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: TryFrom<Vec<u8>>,
    {
        let text = String::deserialize(deserializer)?;
        if text.len() % 2 != 0 || !text.is_ascii() {
            return Err(D::Error::custom("invalid hex"));
        }
        let bytes = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| D::Error::custom("invalid hex"))?;
        T::try_from(bytes).map_err(|_| D::Error::custom("wrong byte length"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    pub fn tune(target: Duration) -> KdfParams {
        let mut params = KdfParams {
            m_kib: TUNE_START_M_KIB,
            t: 1,
            p: 1,
        };
        while params.t < TUNE_MAX_T && time_derivation(params) < target {
            params.t += 1;
        }
        params
    }

    pub fn insecure_fast() -> KdfParams {
        KdfParams {
            m_kib: 8,
            t: 1,
            p: 1,
        }
    }

    fn validate(&self) -> Result<()> {
        let in_bounds = self.m_kib <= MAX_M_KIB && self.t <= MAX_T && self.p <= MAX_P;
        if !in_bounds {
            return Err(corrupt("kdf parameters out of range"));
        }
        self.to_argon2().map(|_| ())
    }

    fn to_argon2(self) -> Result<Argon2<'static>> {
        let params = Params::new(self.m_kib, self.t, self.p, Some(32))
            .map_err(|e| corrupt(format!("kdf parameters: {e}")))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

fn time_derivation(params: KdfParams) -> Duration {
    let started = Instant::now();
    let _ = derive_password_key(b"tune", &[0u8; 16], params);
    started.elapsed()
}

type WrapKey = Zeroizing<[u8; 32]>;

fn derive_password_key(password: &[u8], salt: &[u8; 16], kdf: KdfParams) -> Result<WrapKey> {
    let mut key = Zeroizing::new([0u8; 32]);
    kdf.to_argon2()?
        .hash_password_into(password, salt, &mut key[..])
        .map_err(crypto)?;
    Ok(key)
}

fn derive_recovery_key(code_bytes: &[u8], salt: &[u8; 16]) -> Result<WrapKey> {
    let mut key = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(salt), code_bytes)
        .expand(RECOVERY_INFO, &mut key[..])
        .map_err(crypto)?;
    Ok(key)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SlotKind {
    Password {
        kdf: KdfParams,
        #[serde(with = "hex")]
        salt: [u8; 16],
    },
    Recovery {
        #[serde(with = "hex")]
        salt: [u8; 16],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    #[serde(flatten)]
    pub kind: SlotKind,
    #[serde(with = "hex")]
    pub nonce: [u8; 24],
    #[serde(with = "hex")]
    pub wrapped: Vec<u8>,
}

impl Slot {
    fn wrap(kind: SlotKind, key: &WrapKey, identity: &Identity, recipient: &str) -> Result<Slot> {
        let mut nonce = [0u8; 24];
        rand::fill(&mut nonce);
        let secret = identity.0.to_string();
        let wrapped = cipher(key)
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: secret_bytes(&secret),
                    aad: recipient.as_bytes(),
                },
            )
            .map_err(|_| crypto("failed to wrap identity"))?;
        Ok(Slot {
            kind,
            nonce,
            wrapped,
        })
    }

    fn unwrap(&self, key: &WrapKey, recipient: &str) -> Result<Identity> {
        let plaintext = cipher(key)
            .decrypt(
                &XNonce::from(self.nonce),
                Payload {
                    msg: &self.wrapped,
                    aad: recipient.as_bytes(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| VaultError::BadPassword)?;
        let text = std::str::from_utf8(&plaintext).map_err(|_| corrupt("identity encoding"))?;
        let identity: x25519::Identity = text.parse().map_err(corrupt)?;
        Ok(Identity(identity))
    }
}

fn cipher(key: &WrapKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(&key[..]).expect("wrap key is 32 bytes")
}

fn secret_bytes(secret: &age::secrecy::SecretString) -> &[u8] {
    use age::secrecy::ExposeSecret;
    secret.expose_secret().as_bytes()
}

pub struct Identity(x25519::Identity);

impl Identity {
    pub fn to_age(&self) -> &x25519::Identity {
        &self.0
    }

    pub fn recipient(&self) -> Recipient {
        self.0.to_public()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.0.to_public())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFile {
    pub version: u32,
    pub recipient: String,
    pub slots: Vec<Slot>,
}

impl KeyFile {
    fn validate(&self) -> Result<()> {
        if self.version != KEYFILE_VERSION {
            return Err(corrupt(format!("unsupported version {}", self.version)));
        }
        self.recipient
            .parse::<Recipient>()
            .map_err(|e| corrupt(format!("recipient: {e}")))?;
        for slot in &self.slots {
            if let SlotKind::Password { kdf, .. } = &slot.kind {
                kdf.validate()?;
            }
        }
        Ok(())
    }

    fn password_slots(&self) -> impl Iterator<Item = (&Slot, KdfParams, &[u8; 16])> {
        self.slots.iter().filter_map(|slot| match &slot.kind {
            SlotKind::Password { kdf, salt } => Some((slot, *kdf, salt)),
            SlotKind::Recovery { .. } => None,
        })
    }

    fn recovery_slots(&self) -> impl Iterator<Item = (&Slot, &[u8; 16])> {
        self.slots.iter().filter_map(|slot| match &slot.kind {
            SlotKind::Recovery { salt } => Some((slot, salt)),
            SlotKind::Password { .. } => None,
        })
    }
}

fn encode_recovery_code(bytes: &[u8; RECOVERY_CODE_BYTES]) -> Zeroizing<String> {
    let mut symbols = Zeroizing::new(Vec::with_capacity(RECOVERY_CODE_BYTES * 8 / 5));
    let mut buffer = 0u16;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            symbols.push(CROCKFORD[usize::from((buffer >> bits) & 31)]);
        }
        buffer &= (1 << bits) - 1;
    }
    let mut code = Zeroizing::new(String::new());
    for (i, group) in symbols.chunks(4).enumerate() {
        if i > 0 {
            code.push('-');
        }
        code.extend(group.iter().map(|symbol| char::from(*symbol)));
    }
    code
}

fn decode_recovery_code(code: &str) -> Option<Zeroizing<[u8; RECOVERY_CODE_BYTES]>> {
    let mut bytes = Zeroizing::new([0u8; RECOVERY_CODE_BYTES]);
    let mut buffer = 0u16;
    let mut bits = 0;
    let mut written = 0;
    for c in code.chars().filter(|c| *c != '-' && !c.is_whitespace()) {
        let normalized = match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        };
        let value = CROCKFORD
            .iter()
            .position(|s| char::from(*s) == normalized)?;
        buffer = (buffer << 5) | value as u16;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            *bytes.get_mut(written)? = (buffer >> bits) as u8;
            written += 1;
            buffer &= (1 << bits) - 1;
        }
    }
    (written == RECOVERY_CODE_BYTES && bits == 0).then_some(bytes)
}

fn write_atomically(dir: &Path, name: &str, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join(name)).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn reject_empty(password: &str) -> Result<()> {
    if password.is_empty() {
        return Err(VaultError::EmptyPassword);
    }
    Ok(())
}

fn password_slot(
    password: &str,
    kdf: KdfParams,
    identity: &Identity,
    recipient: &str,
) -> Result<Slot> {
    let mut salt = [0u8; 16];
    rand::fill(&mut salt);
    let key = derive_password_key(password.as_bytes(), &salt, kdf)?;
    Slot::wrap(SlotKind::Password { kdf, salt }, &key, identity, recipient)
}

fn recovery_slot(
    code_bytes: &[u8; RECOVERY_CODE_BYTES],
    identity: &Identity,
    recipient: &str,
) -> Result<Slot> {
    let mut salt = [0u8; 16];
    rand::fill(&mut salt);
    let key = derive_recovery_key(code_bytes, &salt)?;
    Slot::wrap(SlotKind::Recovery { salt }, &key, identity, recipient)
}

#[derive(Debug)]
pub struct Vault {
    dir: PathBuf,
    keyfile: KeyFile,
}

impl Vault {
    pub fn exists(dir: &Path) -> bool {
        dir.join(KEYFILE_NAME).is_file()
    }

    pub fn create(
        dir: &Path,
        password: &str,
        kdf: KdfParams,
        recovery: bool,
    ) -> Result<(Vault, Identity, Option<Zeroizing<String>>)> {
        reject_empty(password)?;
        kdf.validate()?;
        if Self::exists(dir) {
            return Err(VaultError::AlreadyExists);
        }
        std::fs::create_dir_all(dir)?;

        let identity = Identity(x25519::Identity::generate());
        let recipient = identity.recipient().to_string();
        let mut slots = vec![password_slot(password, kdf, &identity, &recipient)?];
        let code = if recovery {
            let mut code_bytes = Zeroizing::new([0u8; RECOVERY_CODE_BYTES]);
            rand::fill(&mut *code_bytes);
            slots.push(recovery_slot(&code_bytes, &identity, &recipient)?);
            Some(encode_recovery_code(&code_bytes))
        } else {
            None
        };

        let vault = Vault {
            dir: dir.to_path_buf(),
            keyfile: KeyFile {
                version: KEYFILE_VERSION,
                recipient,
                slots,
            },
        };
        vault.persist(&vault.keyfile)?;
        Ok((vault, identity, code))
    }

    pub fn open(dir: &Path) -> Result<Vault> {
        let bytes = std::fs::read(dir.join(KEYFILE_NAME))?;
        let keyfile: KeyFile = serde_json::from_slice(&bytes).map_err(corrupt)?;
        keyfile.validate()?;
        Ok(Vault {
            dir: dir.to_path_buf(),
            keyfile,
        })
    }

    pub fn recipient(&self) -> Recipient {
        self.keyfile
            .recipient
            .parse()
            .expect("recipient validated when the key file was loaded")
    }

    pub fn unlock(&self, password: &str) -> Result<Identity> {
        reject_empty(password)?;
        for (slot, kdf, salt) in self.keyfile.password_slots() {
            let key = derive_password_key(password.as_bytes(), salt, kdf)?;
            if let Some(identity) = self.try_slot(slot, &key)? {
                return Ok(identity);
            }
        }
        Err(VaultError::BadPassword)
    }

    pub fn unlock_recovery(&self, code: &str) -> Result<Identity> {
        let code_bytes = decode_recovery_code(code).ok_or(VaultError::BadPassword)?;
        for (slot, salt) in self.keyfile.recovery_slots() {
            let key = derive_recovery_key(code_bytes.as_ref(), salt)?;
            if let Some(identity) = self.try_slot(slot, &key)? {
                return Ok(identity);
            }
        }
        Err(VaultError::BadPassword)
    }

    pub fn change_password(
        &mut self,
        identity: &Identity,
        new: &str,
        kdf: KdfParams,
    ) -> Result<()> {
        reject_empty(new)?;
        kdf.validate()?;
        let recipient = &self.keyfile.recipient;
        if identity.recipient().to_string() != *recipient {
            return Err(VaultError::IdentityMismatch);
        }
        let replacement = password_slot(new, kdf, identity, recipient)?;
        let mut keyfile = self.keyfile.clone();
        keyfile
            .slots
            .retain(|slot| matches!(slot.kind, SlotKind::Recovery { .. }));
        keyfile.slots.insert(0, replacement);
        self.persist(&keyfile)?;
        self.keyfile = keyfile;
        Ok(())
    }

    fn try_slot(&self, slot: &Slot, key: &WrapKey) -> Result<Option<Identity>> {
        let identity = match slot.unwrap(key, &self.keyfile.recipient) {
            Ok(identity) => identity,
            Err(VaultError::BadPassword) => return Ok(None),
            Err(other) => return Err(other),
        };
        if identity.recipient().to_string() != self.keyfile.recipient {
            return Err(corrupt("slot does not match recipient"));
        }
        Ok(Some(identity))
    }

    fn persist(&self, keyfile: &KeyFile) -> Result<()> {
        let json = serde_json::to_vec_pretty(keyfile).map_err(corrupt)?;
        write_atomically(&self.dir, KEYFILE_NAME, &json)
    }
}

pub fn seal_small(recipient: &Recipient, plaintext: &[u8]) -> Result<Vec<u8>> {
    age::encrypt(recipient, plaintext).map_err(crypto)
}

pub fn open_small(identity: &Identity, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    age::decrypt(identity.to_age(), ciphertext)
        .map(Zeroizing::new)
        .map_err(crypto)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use regex::Regex;

    use super::*;

    fn fast_vault(
        password: &str,
        recovery: bool,
    ) -> (
        tempfile::TempDir,
        Vault,
        Identity,
        Option<Zeroizing<String>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (vault, identity, code) =
            Vault::create(dir.path(), password, KdfParams::insecure_fast(), recovery).unwrap();
        (dir, vault, identity, code)
    }

    #[test]
    fn create_then_unlock() {
        let (dir, vault, identity, code) = fast_vault("hunter2", false);
        assert!(code.is_none());
        assert!(Vault::exists(dir.path()));
        let reopened = Vault::open(dir.path()).unwrap();
        let unlocked = reopened.unlock("hunter2").unwrap();
        assert_eq!(
            unlocked.recipient().to_string(),
            identity.recipient().to_string()
        );
        assert_eq!(
            reopened.recipient().to_string(),
            vault.recipient().to_string()
        );
    }

    #[test]
    fn wrong_password_is_bad_password() {
        let (_dir, vault, _, _) = fast_vault("hunter2", false);
        assert!(matches!(
            vault.unlock("hunter3"),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn unicode_password_roundtrip() {
        let (_dir, vault, identity, _) = fast_vault("pässwörd-密码", false);
        let unlocked = vault.unlock("pässwörd-密码").unwrap();
        assert_eq!(
            unlocked.recipient().to_string(),
            identity.recipient().to_string()
        );
        assert!(matches!(
            vault.unlock("passwörd-密码"),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn empty_password_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = Vault::create(dir.path(), "", KdfParams::insecure_fast(), false);
        assert!(matches!(result, Err(VaultError::EmptyPassword)));
        assert!(!Vault::exists(dir.path()));

        let (_dir, mut vault, identity, _) = fast_vault("hunter2", false);
        let change = vault.change_password(&identity, "", KdfParams::insecure_fast());
        assert!(matches!(change, Err(VaultError::EmptyPassword)));
        assert!(matches!(vault.unlock(""), Err(VaultError::EmptyPassword)));
        vault.unlock("hunter2").unwrap();
    }

    #[test]
    fn recovery_code_unlocks() {
        let (_dir, vault, identity, code) = fast_vault("hunter2", true);
        let code = code.expect("recovery code requested");
        let format = Regex::new(r"^[0-9A-Z]{4}(-[0-9A-Z]{4}){7}$").unwrap();
        assert!(format.is_match(&code), "unexpected format: {}", code.len());

        let unlocked = vault.unlock_recovery(&code).unwrap();
        assert_eq!(
            unlocked.recipient().to_string(),
            identity.recipient().to_string()
        );

        let sloppy = code.to_lowercase().replace('-', " ");
        vault.unlock_recovery(&sloppy).unwrap();
    }

    #[test]
    fn wrong_or_malformed_recovery_code_is_bad_password() {
        let (_dir, vault, _, code) = fast_vault("hunter2", true);
        let mut other = code.unwrap().to_string();
        other.replace_range(0..1, if other.starts_with('0') { "1" } else { "0" });
        assert!(matches!(
            vault.unlock_recovery(&other),
            Err(VaultError::BadPassword)
        ));
        assert!(matches!(
            vault.unlock_recovery("nope"),
            Err(VaultError::BadPassword)
        ));
        assert!(matches!(
            vault.unlock_recovery(""),
            Err(VaultError::BadPassword)
        ));

        let (_dir, without_recovery, _, _) = fast_vault("hunter2", false);
        let valid_shape = "0000-0000-0000-0000-0000-0000-0000-0000";
        assert!(matches!(
            without_recovery.unlock_recovery(valid_shape),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn change_password_keeps_identity() {
        let (dir, mut vault, identity, code) = fast_vault("old-password", true);
        let sealed = seal_small(&vault.recipient(), b"before the change").unwrap();
        let recipient_before = vault.recipient().to_string();

        vault
            .change_password(&identity, "new-password", KdfParams::insecure_fast())
            .unwrap();

        assert_eq!(vault.recipient().to_string(), recipient_before);
        assert!(matches!(
            vault.unlock("old-password"),
            Err(VaultError::BadPassword)
        ));
        let unlocked = vault.unlock("new-password").unwrap();
        assert_eq!(
            &*open_small(&unlocked, &sealed).unwrap(),
            b"before the change"
        );
        vault.unlock_recovery(&code.unwrap()).unwrap();

        let reopened = Vault::open(dir.path()).unwrap();
        assert!(matches!(
            reopened.unlock("old-password"),
            Err(VaultError::BadPassword)
        ));
        reopened.unlock("new-password").unwrap();
    }

    #[test]
    fn change_password_rejects_foreign_identity() {
        let (_dir, mut vault, _, _) = fast_vault("hunter2", false);
        let (_other_dir, _, foreign, _) = fast_vault("other", false);
        let result = vault.change_password(&foreign, "new", KdfParams::insecure_fast());
        assert!(matches!(result, Err(VaultError::IdentityMismatch)));
        vault.unlock("hunter2").unwrap();
    }

    #[test]
    fn tampered_slot_fails() {
        let (_dir, mut vault, _, code) = fast_vault("hunter2", true);
        let code = code.unwrap();
        vault.keyfile.slots[0].wrapped[3] ^= 0x01;
        assert!(matches!(
            vault.unlock("hunter2"),
            Err(VaultError::BadPassword)
        ));
        vault.unlock_recovery(&code).unwrap();

        vault.keyfile.slots[1].wrapped[3] ^= 0x01;
        assert!(matches!(
            vault.unlock_recovery(&code),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn truncated_slot_fails() {
        let (_dir, mut vault, _, _) = fast_vault("hunter2", false);
        vault.keyfile.slots[0].wrapped.truncate(4);
        assert!(matches!(
            vault.unlock("hunter2"),
            Err(VaultError::BadPassword)
        ));
        vault.keyfile.slots[0].wrapped.clear();
        assert!(matches!(
            vault.unlock("hunter2"),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn tampered_recipient_fails_unlock() {
        let (_dir, mut vault, _, _) = fast_vault("hunter2", false);
        let (_other_dir, other, _, _) = fast_vault("other", false);
        vault.keyfile.recipient = other.recipient().to_string();
        assert!(vault.unlock("hunter2").is_err());
    }

    #[test]
    fn keyfile_mode_0600() {
        let (dir, mut vault, identity, _) = fast_vault("hunter2", false);
        let path = dir.path().join("vault.json");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);

        vault
            .change_password(&identity, "another", KdfParams::insecure_fast())
            .unwrap();
        assert_eq!(mode(&path), 0o600);
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files must not remain");
    }

    #[test]
    fn create_refuses_to_overwrite_existing_vault() {
        let (dir, vault, _, _) = fast_vault("hunter2", false);
        let result = Vault::create(dir.path(), "other", KdfParams::insecure_fast(), false);
        assert!(matches!(result, Err(VaultError::AlreadyExists)));
        vault.unlock("hunter2").unwrap();
    }

    #[test]
    fn open_rejects_hostile_keyfiles() {
        let (dir, _, _, _) = fast_vault("hunter2", false);
        let path = dir.path().join("vault.json");
        let good = std::fs::read_to_string(&path).unwrap();

        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Vault::open(dir.path()),
            Err(VaultError::Corrupt(_))
        ));

        std::fs::write(&path, good.replace("\"m_kib\": 8", "\"m_kib\": 4294967295")).unwrap();
        assert!(matches!(
            Vault::open(dir.path()),
            Err(VaultError::Corrupt(_))
        ));

        std::fs::write(&path, good.replace("\"version\": 1", "\"version\": 2")).unwrap();
        assert!(matches!(
            Vault::open(dir.path()),
            Err(VaultError::Corrupt(_))
        ));
    }

    #[test]
    fn open_missing_vault_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Vault::exists(dir.path()));
        assert!(matches!(Vault::open(dir.path()), Err(VaultError::Io(_))));
    }

    #[test]
    fn keyfile_json_roundtrip_is_stable() {
        let (dir, vault, _, _) = fast_vault("hunter2", true);
        let text = std::fs::read_to_string(dir.path().join("vault.json")).unwrap();
        let parsed: KeyFile = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, vault.keyfile);
        assert_eq!(parsed.version, 1);
        assert!(text.contains("\"type\": \"password\""));
        assert!(text.contains("\"type\": \"recovery\""));
    }

    #[test]
    fn nonces_are_unique_per_wrap() {
        let (_dir, vault, identity, _) = fast_vault("hunter2", false);
        let recipient = vault.keyfile.recipient.clone();
        let first =
            password_slot("hunter2", KdfParams::insecure_fast(), &identity, &recipient).unwrap();
        let second =
            password_slot("hunter2", KdfParams::insecure_fast(), &identity, &recipient).unwrap();
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.wrapped, second.wrapped);
    }

    #[test]
    fn wrapped_identity_is_bound_to_the_recipient_string() {
        let (_dir, vault, _, _) = fast_vault("hunter2", false);
        let slot = &vault.keyfile.slots[0];
        let SlotKind::Password { kdf, salt } = &slot.kind else {
            panic!("first slot is the password slot");
        };
        let key = derive_password_key(b"hunter2", salt, *kdf).unwrap();
        slot.unwrap(&key, &vault.keyfile.recipient).unwrap();
        let other = x25519::Identity::generate().to_public().to_string();
        assert!(matches!(
            slot.unwrap(&key, &other),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn identity_debug_does_not_print_the_key() {
        let (_dir, _, identity, _) = fast_vault("hunter2", false);
        let secret_text = identity.to_age().to_string();
        let secret = age::secrecy::ExposeSecret::expose_secret(&secret_text);
        assert!(!format!("{identity:?}").contains(secret));
    }

    #[test]
    fn seal_open_small_roundtrip() {
        let (_dir, vault, identity, _) = fast_vault("hunter2", false);
        let sealed = seal_small(&vault.recipient(), b"hello capricorn").unwrap();
        assert_ne!(&sealed[..], b"hello capricorn");
        assert_eq!(
            &*open_small(&identity, &sealed).unwrap(),
            b"hello capricorn"
        );

        let (_dir2, _, stranger, _) = fast_vault("other", false);
        assert!(open_small(&stranger, &sealed).is_err());
        assert!(open_small(&identity, b"garbage").is_err());
    }

    #[test]
    fn recovery_code_encoding_roundtrips() {
        let bytes: [u8; RECOVERY_CODE_BYTES] = std::array::from_fn(|i| (i * 37 + 11) as u8);
        let code = encode_recovery_code(&bytes);
        assert_eq!(code.len(), 39);
        assert_eq!(&*decode_recovery_code(&code).unwrap(), &bytes);
    }

    #[test]
    fn tune_stays_within_bounds() {
        let params = KdfParams::tune(Duration::ZERO);
        assert_eq!((params.m_kib, params.t, params.p), (256 * 1024, 1, 1));
    }
}
