//! A file of secrets encrypted with a key derived from a master password, for
//! systems without a credential service.
//!
//! Format (all integers little-endian):
//!
//! ```text
//! magic "FERGESEC" | version u8 | m_cost u32 | t_cost u32 | p_cost u32
//! | salt [16] | nonce [24] | ciphertext with Poly1305 tag
//! ```
//!
//! The key is Argon2id(master password, salt) with the stored cost
//! parameters; the cipher is XChaCha20-Poly1305 with a fresh random nonce for
//! every write. The header is authenticated as associated data, so cost
//! parameters cannot be tampered with unnoticed. The plaintext is a sequence
//! of `len u32 | account | len u32 | secret` records.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use secrecy::{ExposeSecret as _, SecretString};
use zeroize::Zeroizing;

use super::{Result, SecretError, SecretId, SecretStore};

const MAGIC: &[u8; 8] = b"FERGESEC";
const VERSION: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + 1 + 3 * 4 + SALT_LEN + NONCE_LEN;

/// Larger files are rejected before decryption.
const MAX_FILE_LEN: usize = 16 * 1024 * 1024;

/// Argon2id costs for new files: 64 MiB, three passes (RFC 9106's second
/// recommended option). Unlocking takes a few hundred milliseconds.
const DEFAULT_KDF: KdfParams = KdfParams {
    m_cost: 64 * 1024,
    t_cost: 3,
    p_cost: 1,
};

/// Upper bounds for costs read from a file, so a crafted file cannot make
/// unlocking exhaust memory or time.
const MAX_M_COST: u32 = 1024 * 1024;
const MAX_T_COST: u32 = 16;
const MAX_P_COST: u32 = 8;

/// Argon2's memory (KiB), time (passes) and parallelism costs, named as in RFC 9106.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(clippy::struct_field_names, reason = "the RFC's names")]
struct KdfParams {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

impl KdfParams {
    fn derive(self, master: &SecretString, salt: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        let invalid = |error: argon2::Error| SecretError::Corrupt(error.to_string());
        let params =
            Params::new(self.m_cost, self.t_cost, self.p_cost, Some(KEY_LEN)).map_err(invalid)?;
        let mut key = Zeroizing::new([0; KEY_LEN]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(master.expose_secret().as_bytes(), salt, key.as_mut())
            .map_err(invalid)?;
        Ok(key)
    }

    fn is_reasonable(self) -> bool {
        (1..=MAX_P_COST).contains(&self.p_cost)
            && (1..=MAX_T_COST).contains(&self.t_cost)
            && (8 * self.p_cost..=MAX_M_COST).contains(&self.m_cost)
    }
}

/// The decrypted state of an unlocked store.
struct Unlocked {
    key: Zeroizing<[u8; KEY_LEN]>,
    salt: [u8; SALT_LEN],
    kdf: KdfParams,
    entries: BTreeMap<String, SecretString>,
}

/// Secrets in a file encrypted with a master password.
///
/// The store starts locked. [`unlock`](Self::unlock) decrypts an existing
/// file or creates a new one. Every read and write re-reads the file, so
/// several processes can share it without losing each other's changes.
pub struct EncryptedFileStore {
    path: PathBuf,
    state: Mutex<Option<Unlocked>>,
}

impl std::fmt::Debug for EncryptedFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedFileStore")
            .field("path", &self.path)
            .field("unlocked", &self.is_unlocked())
            .finish_non_exhaustive()
    }
}

impl EncryptedFileStore {
    /// A locked store backed by `path`, which need not exist yet.
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            state: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file exists; if not, [`unlock`](Self::unlock) creates it
    /// and the UI should ask for the new master password twice.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn is_unlocked(&self) -> bool {
        self.state().is_some()
    }

    /// Decrypts the file with `master`, or creates an empty one protected by
    /// `master` if there is no file yet.
    pub fn unlock(&self, master: &SecretString) -> Result<()> {
        if master.expose_secret().is_empty() {
            return Err(SecretError::Invalid(
                "the master password must not be empty".into(),
            ));
        }
        // The file lock is released before the state is taken, the order
        // `with_fresh_state` takes them in reverse; holding both here could
        // deadlock against it.
        let guard = self.lock_file()?;
        let unlocked = if let Some(data) = self.read_file()? {
            let header = Header::parse(&data)?;
            let key = header.kdf.derive(master, &header.salt)?;
            let entries = decrypt(&data, &header, &key)?;
            Unlocked {
                key,
                salt: header.salt,
                kdf: header.kdf,
                entries,
            }
        } else {
            let mut salt = [0; SALT_LEN];
            fill_random(&mut salt)?;
            let unlocked = Unlocked {
                key: DEFAULT_KDF.derive(master, &salt)?,
                salt,
                kdf: DEFAULT_KDF,
                entries: BTreeMap::new(),
            };
            self.write_file(&unlocked)?;
            unlocked
        };
        drop(guard);
        *self.state() = Some(unlocked);
        Ok(())
    }

    /// Forgets the key and the decrypted secrets.
    pub fn lock(&self) {
        *self.state() = None;
    }

    fn state(&self) -> MutexGuard<'_, Option<Unlocked>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the unlocked state after merging in changes that other
    /// processes wrote to the file.
    fn with_fresh_state<T>(&self, f: impl FnOnce(&mut Unlocked) -> Result<T>) -> Result<T> {
        let mut state = self.state();
        // Another ferge may read, change and write the file at the same
        // time; holding the lock from reading to writing keeps either's
        // change from being lost.
        let _guard = self.lock_file()?;
        let unlocked = state.as_mut().ok_or(SecretError::Locked)?;
        if let Some(data) = self.read_file()? {
            let header = Header::parse(&data)?;
            if header.salt != unlocked.salt || header.kdf != unlocked.kdf {
                // Re-keyed elsewhere; the cached key no longer fits.
                *state = None;
                return Err(SecretError::Locked);
            }
            unlocked.entries = decrypt(&data, &header, &unlocked.key)?;
        }
        f(unlocked)
    }

    /// Takes the advisory lock on `<file>.lock` that every ferge holds
    /// while it reads and writes the store; released when dropped.
    fn lock_file(&self) -> Result<std::fs::File> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        create_private_dir(dir).map_err(|source| self.io_error(source))?;
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(dir.join(name))
            .map_err(|source| self.io_error(source))?;
        file.lock().map_err(|source| self.io_error(source))?;
        Ok(file)
    }

    fn read_file(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        match std::fs::read(&self.path) {
            Ok(data) if data.len() > MAX_FILE_LEN => {
                Err(SecretError::Corrupt("the file is too large".into()))
            }
            Ok(data) => Ok(Some(Zeroizing::new(data))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(self.io_error(source)),
        }
    }

    /// Encrypts `unlocked` and atomically replaces the file.
    fn write_file(&self, unlocked: &Unlocked) -> Result<()> {
        let mut nonce = [0; NONCE_LEN];
        fill_random(&mut nonce)?;
        let mut data = Vec::with_capacity(HEADER_LEN + 256);
        data.extend_from_slice(MAGIC);
        data.push(VERSION);
        data.extend_from_slice(&unlocked.kdf.m_cost.to_le_bytes());
        data.extend_from_slice(&unlocked.kdf.t_cost.to_le_bytes());
        data.extend_from_slice(&unlocked.kdf.p_cost.to_le_bytes());
        data.extend_from_slice(&unlocked.salt);
        data.extend_from_slice(&nonce);

        let plaintext = encode_entries(&unlocked.entries)?;
        let cipher = XChaCha20Poly1305::new_from_slice(unlocked.key.as_ref())
            .map_err(|_| SecretError::Invalid("bad key length".into()))?;
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plaintext,
                    aad: &data,
                },
            )
            .map_err(|_| SecretError::Invalid("encryption failed".into()))?;
        data.extend_from_slice(&ciphertext);

        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        create_private_dir(dir).map_err(|source| self.io_error(source))?;
        // Written next to the target and renamed over it, so a crash never
        // leaves a truncated store. The temporary file is created owner-only.
        let mut file =
            tempfile::NamedTempFile::new_in(dir).map_err(|source| self.io_error(source))?;
        file.write_all(&data)
            .and_then(|()| file.as_file().sync_all())
            .map_err(|source| self.io_error(source))?;
        file.persist(&self.path)
            .map_err(|error| self.io_error(error.error))?;
        Ok(())
    }

    fn io_error(&self, source: std::io::Error) -> SecretError {
        SecretError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

impl SecretStore for EncryptedFileStore {
    fn name(&self) -> &'static str {
        "encrypted file"
    }

    fn get(&self, id: &SecretId) -> Result<Option<SecretString>> {
        self.with_fresh_state(|unlocked| Ok(unlocked.entries.get(&id.account()).cloned()))
    }

    fn set(&self, id: &SecretId, secret: &SecretString) -> Result<()> {
        self.with_fresh_state(|unlocked| {
            unlocked.entries.insert(id.account(), secret.clone());
            self.write_file(unlocked)
        })
    }

    fn delete(&self, id: &SecretId) -> Result<()> {
        self.with_fresh_state(|unlocked| {
            if unlocked.entries.remove(&id.account()).is_some() {
                self.write_file(unlocked)?;
            }
            Ok(())
        })
    }

    fn as_file_store(&self) -> Option<&EncryptedFileStore> {
        Some(self)
    }
}

struct Header {
    kdf: KdfParams,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
}

impl Header {
    fn parse(data: &[u8]) -> Result<Self> {
        let corrupt = |reason: &str| SecretError::Corrupt(reason.into());
        if data.len() < HEADER_LEN + TAG_LEN || !data.starts_with(MAGIC) {
            return Err(corrupt("not a ferge password store"));
        }
        if data[MAGIC.len()] != VERSION {
            return Err(corrupt("written by an unsupported version of ferge"));
        }
        let mut cursor = &data[MAGIC.len() + 1..HEADER_LEN];
        let kdf = KdfParams {
            m_cost: take_u32(&mut cursor).ok_or_else(|| corrupt("truncated header"))?,
            t_cost: take_u32(&mut cursor).ok_or_else(|| corrupt("truncated header"))?,
            p_cost: take_u32(&mut cursor).ok_or_else(|| corrupt("truncated header"))?,
        };
        if !kdf.is_reasonable() {
            return Err(corrupt("unsupported key derivation parameters"));
        }
        let (salt, nonce) = cursor.split_at(SALT_LEN);
        Ok(Self {
            kdf,
            salt: salt.try_into().map_err(|_| corrupt("truncated header"))?,
            nonce: nonce.try_into().map_err(|_| corrupt("truncated header"))?,
        })
    }
}

fn decrypt(
    data: &[u8],
    header: &Header,
    key: &[u8; KEY_LEN],
) -> Result<BTreeMap<String, SecretString>> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| SecretError::Invalid("bad key length".into()))?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &XNonce::from(header.nonce),
                Payload {
                    msg: &data[HEADER_LEN..],
                    aad: &data[..HEADER_LEN],
                },
            )
            .map_err(|_| SecretError::WrongPassword)?,
    );
    decode_entries(&plaintext)
}

fn encode_entries(entries: &BTreeMap<String, SecretString>) -> Result<Zeroizing<Vec<u8>>> {
    // Sized exactly up front: growing the buffer would leave copies of the
    // secrets behind in freed memory that `Zeroizing` never sees.
    let size = entries
        .iter()
        .map(|(account, secret)| 8 + account.len() + secret.expose_secret().len())
        .sum();
    let mut plaintext = Zeroizing::new(Vec::with_capacity(size));
    for (account, secret) in entries {
        for field in [account.as_bytes(), secret.expose_secret().as_bytes()] {
            let len = u32::try_from(field.len())
                .map_err(|_| SecretError::Invalid("a secret is too long".into()))?;
            plaintext.extend_from_slice(&len.to_le_bytes());
            plaintext.extend_from_slice(field);
        }
    }
    debug_assert_eq!(plaintext.len(), plaintext.capacity());
    Ok(plaintext)
}

fn decode_entries(mut data: &[u8]) -> Result<BTreeMap<String, SecretString>> {
    let corrupt = || SecretError::Corrupt("malformed contents".into());
    let mut entries = BTreeMap::new();
    while !data.is_empty() {
        let account = take_field(&mut data).ok_or_else(corrupt)?;
        let secret = take_field(&mut data).ok_or_else(corrupt)?;
        let account = std::str::from_utf8(account).map_err(|_| corrupt())?;
        let secret = std::str::from_utf8(secret).map_err(|_| corrupt())?;
        entries.insert(account.to_owned(), SecretString::from(secret));
    }
    Ok(entries)
}

fn take_u32(data: &mut &[u8]) -> Option<u32> {
    let (bytes, rest) = data.split_first_chunk::<4>()?;
    *data = rest;
    Some(u32::from_le_bytes(*bytes))
}

fn take_field<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(take_u32(data)?).ok()?;
    if len > data.len() {
        return None;
    }
    let (field, rest) = data.split_at(len);
    *data = rest;
    Some(field)
}

fn fill_random(buffer: &mut [u8]) -> Result<()> {
    getrandom::fill(buffer)
        .map_err(|error| SecretError::Unavailable(format!("no randomness available: {error}")))
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        // A folder made earlier, by hand or by an older version, may be
        // open to others; it holds only ferge's data, so it is closed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir)?.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode & 0o700))?;
            }
        }
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap costs so tests stay fast; the format is the same.
    fn fast_store(path: &Path) -> EncryptedFileStore {
        let store = EncryptedFileStore::new(path);
        let mut salt = [0; SALT_LEN];
        fill_random(&mut salt).unwrap();
        let kdf = KdfParams {
            m_cost: 64,
            t_cost: 1,
            p_cost: 1,
        };
        let unlocked = Unlocked {
            key: kdf.derive(&secret("master"), &salt).unwrap(),
            salt,
            kdf,
            entries: BTreeMap::new(),
        };
        store.write_file(&unlocked).unwrap();
        store
    }

    fn secret(text: &str) -> SecretString {
        SecretString::from(text)
    }

    fn id(host: &str) -> SecretId {
        SecretId::Password {
            user: "me".into(),
            host: host.into(),
            port: 22,
        }
    }

    #[test]
    fn concurrent_stores_keep_each_others_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.bin");
        drop(fast_store(&path));
        // Two ferge processes, as far as the file is concerned.
        let writers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|prefix| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = EncryptedFileStore::new(&path);
                    store.unlock(&secret("master")).unwrap();
                    for index in 0..20 {
                        store
                            .set(&id(&format!("{prefix}{index}")), &secret("pw"))
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let store = EncryptedFileStore::new(&path);
        store.unlock(&secret("master")).unwrap();
        for prefix in ["a", "b"] {
            for index in 0..20 {
                assert!(
                    store
                        .get(&id(&format!("{prefix}{index}")))
                        .unwrap()
                        .is_some(),
                    "{prefix}{index} was lost"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_open_folder_is_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("ferge");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(fast_store(&data.join("secrets.bin")));
        let mode = std::fs::metadata(&data).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn stores_and_reloads_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.bin");
        let store = fast_store(&path);
        assert!(matches!(store.get(&id("a")), Err(SecretError::Locked)));
        store.unlock(&secret("master")).unwrap();
        store.set(&id("a"), &secret("pw-a")).unwrap();
        store
            .set(&SecretId::KeyPassphrase("/k".into()), &secret("ü ✓"))
            .unwrap();
        store.set(&id("a"), &secret("pw-a2")).unwrap();
        store.delete(&id("missing")).unwrap();

        let other = EncryptedFileStore::new(&path);
        other.unlock(&secret("master")).unwrap();
        assert_eq!(
            other.get(&id("a")).unwrap().unwrap().expose_secret(),
            "pw-a2"
        );
        assert_eq!(
            other
                .get(&SecretId::KeyPassphrase("/k".into()))
                .unwrap()
                .unwrap()
                .expose_secret(),
            "ü ✓"
        );
        // Changes of one process are visible to the other.
        other.set(&id("b"), &secret("pw-b")).unwrap();
        store.delete(&id("a")).unwrap();
        assert!(other.get(&id("a")).unwrap().is_none());
        assert!(store.get(&id("b")).unwrap().is_some());

        store.lock();
        assert!(matches!(store.get(&id("b")), Err(SecretError::Locked)));
    }

    #[test]
    fn rejects_wrong_passwords_and_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.bin");
        let store = fast_store(&path);
        store.unlock(&secret("master")).unwrap();
        store.set(&id("a"), &secret("pw")).unwrap();

        let fresh = EncryptedFileStore::new(&path);
        assert!(matches!(
            fresh.unlock(&secret("wrong")),
            Err(SecretError::WrongPassword)
        ));
        assert!(fresh.unlock(&secret("")).is_err());

        let original = std::fs::read(&path).unwrap();
        // Flipping any bit, in the header or the ciphertext, is detected.
        for position in [MAGIC.len() + 2, HEADER_LEN - 1, original.len() - 1] {
            let mut tampered = original.clone();
            tampered[position] ^= 1;
            std::fs::write(&path, &tampered).unwrap();
            assert!(
                EncryptedFileStore::new(&path)
                    .unlock(&secret("master"))
                    .is_err(),
                "byte {position}"
            );
        }
        std::fs::write(&path, b"FERGESEC").unwrap();
        assert!(matches!(
            EncryptedFileStore::new(&path).unlock(&secret("master")),
            Err(SecretError::Corrupt(_))
        ));
    }

    #[test]
    fn refuses_unreasonable_costs() {
        let mut header = Vec::from(&MAGIC[..]);
        header.push(VERSION);
        for value in [u32::MAX, 1, 1] {
            header.extend_from_slice(&value.to_le_bytes());
        }
        header.resize(HEADER_LEN + TAG_LEN, 0);
        assert!(matches!(
            Header::parse(&header),
            Err(SecretError::Corrupt(_))
        ));
    }

    #[test]
    fn creates_a_store_on_first_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/secrets.bin");
        let store = EncryptedFileStore::new(&path);
        assert!(!store.exists());
        store.unlock(&secret("new master")).unwrap();
        assert!(store.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the file must be private");
        }
        let header = Header::parse(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(header.kdf, DEFAULT_KDF);
    }

    #[test]
    fn decodes_only_well_formed_contents() {
        let mut entries = BTreeMap::new();
        entries.insert("k".to_owned(), secret("v"));
        let encoded = encode_entries(&entries).unwrap();
        assert_eq!(decode_entries(&encoded).unwrap().len(), 1);
        assert!(decode_entries(&encoded[..encoded.len() - 1]).is_err());
        assert!(decode_entries(&[255, 255, 255, 255]).is_err());
    }
}
