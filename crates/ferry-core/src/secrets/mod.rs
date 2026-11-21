//! Saved passwords and key passphrases.
//!
//! Secrets live in the platform's credential store: the Secret Service on
//! Linux (GNOME Keyring, KeePassXC, KWallet) and the Keychain on macOS. Where
//! no such service is available, an [`EncryptedFileStore`] protected by a
//! master password takes its place.
//!
//! Every operation may block (D-Bus round trips, key derivation, disk I/O) and
//! must not run on a UI thread.

mod file;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod keyring;
mod memory;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use secrecy::SecretString;

pub use self::file::EncryptedFileStore;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use self::keyring::KeyringStore;
pub use self::memory::MemoryStore;

/// Environment variable that forces a store: `keyring` or `file`.
pub const STORE_OVERRIDE_VAR: &str = "FERRY_SECRET_STORE";

/// Errors of secret stores.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The platform's credential service failed or refused access.
    #[error("the credential store is not available: {0}")]
    Unavailable(String),
    /// The encrypted file store has not been unlocked yet.
    #[error("the password store is locked")]
    Locked,
    /// The master password did not decrypt the file store.
    #[error("wrong master password, or the password store is damaged")]
    WrongPassword,
    #[error("the password store is damaged: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Invalid(String),
    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = SecretError> = std::result::Result<T, E>;

/// What a secret unlocks.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SecretId {
    /// The password of `user` on an SSH server.
    Password {
        user: String,
        host: String,
        port: u16,
    },
    /// The passphrase of a private key file.
    KeyPassphrase(PathBuf),
    /// The password `sudo` asks `user` for on an SSH server, to work as root.
    SudoPassword {
        user: String,
        host: String,
        port: u16,
    },
}

impl SecretId {
    /// The account name under which the secret is stored, unique per id.
    fn account(&self) -> String {
        match self {
            Self::Password { user, host, port } => format!("password:{user}@{host}:{port}"),
            Self::KeyPassphrase(path) => format!("passphrase:{}", path.display()),
            Self::SudoPassword { user, host, port } => format!("sudo:{user}@{host}:{port}"),
        }
    }

    /// A human-readable description, e.g. for keyring managers.
    fn label(&self) -> String {
        match self {
            Self::Password { user, host, port } => {
                format!("Ferry: password for {user}@{host}:{port}")
            }
            Self::KeyPassphrase(path) => format!("Ferry: passphrase for {}", path.display()),
            Self::SudoPassword { user, host, port } => {
                format!("Ferry: sudo password for {user}@{host}:{port}")
            }
        }
    }
}

impl fmt::Display for SecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// A place to keep secrets.
pub trait SecretStore: Send + Sync + fmt::Debug {
    /// Name for the UI, e.g. "Secret Service".
    fn name(&self) -> &'static str;

    /// The stored secret, or `None` if there is none.
    fn get(&self, id: &SecretId) -> Result<Option<SecretString>>;

    /// Stores `secret`, replacing an earlier one.
    fn set(&self, id: &SecretId, secret: &SecretString) -> Result<()>;

    /// Removes the secret. Removing a missing secret is not an error.
    fn delete(&self, id: &SecretId) -> Result<()>;

    /// The file store behind this store, if it is one; it needs to be
    /// unlocked with a master password before use.
    fn as_file_store(&self) -> Option<&EncryptedFileStore> {
        None
    }
}

/// Opens the best available store: the platform's credential service, or the
/// encrypted file at `fallback` when there is none. [`STORE_OVERRIDE_VAR`]
/// selects one explicitly.
pub fn open_default(fallback: &Path) -> Arc<dyn SecretStore> {
    let file_store = || -> Arc<dyn SecretStore> { Arc::new(EncryptedFileStore::new(fallback)) };
    match std::env::var(STORE_OVERRIDE_VAR).as_deref() {
        Ok("file") => return file_store(),
        Ok("keyring") | Err(_) => {}
        Ok(other) => tracing::warn!("ignoring {STORE_OVERRIDE_VAR}={other:?}"),
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match KeyringStore::open() {
        Ok(store) => return Arc::new(store),
        Err(error) => {
            tracing::info!(%error, "no platform credential store, using the encrypted file");
        }
    }
    file_store()
}

/// `ferry/secrets.bin` in the platform's data directory.
pub fn default_fallback_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("ferry").join("secrets.bin"))
}
