//! The platform's credential service through keyring-core.

use std::collections::HashMap;
use std::sync::Arc;

use keyring_core::{CredentialStore, Entry, Error as KeyringError};
use secrecy::{ExposeSecret as _, SecretString};

use super::{Result, SecretError, SecretId, SecretStore};

/// Service name under which all of Ferry's secrets are filed.
const SERVICE: &str = "dev.ferry.Ferry";

/// The Secret Service (Linux) or the login Keychain (macOS).
#[derive(Debug)]
pub struct KeyringStore {
    store: Arc<CredentialStore>,
}

impl KeyringStore {
    /// Connects to the platform's credential service. Fails if there is none,
    /// e.g. no Secret Service provider on the session bus.
    ///
    /// On Linux this must be called within a tokio runtime, as must every
    /// later operation on the store.
    pub fn open() -> Result<Self> {
        #[cfg(target_os = "linux")]
        let store: Arc<CredentialStore> =
            zbus_secret_service_keyring_store::Store::new().map_err(unavailable)?;
        #[cfg(target_os = "macos")]
        let store: Arc<CredentialStore> =
            apple_native_keyring_store::keychain::Store::new().map_err(unavailable)?;
        Ok(Self { store })
    }

    fn entry(&self, id: &SecretId) -> Result<Entry> {
        let label = id.label();
        let mut modifiers = HashMap::new();
        // Only the Secret Service shows labels; the Keychain accepts none.
        if cfg!(target_os = "linux") {
            modifiers.insert("label", label.as_str());
        }
        self.store
            .build(SERVICE, &id.account(), Some(&modifiers))
            .map_err(unavailable)
    }
}

impl SecretStore for KeyringStore {
    fn name(&self) -> &'static str {
        if cfg!(target_os = "macos") {
            "Keychain"
        } else {
            "Secret Service"
        }
    }

    fn get(&self, id: &SecretId) -> Result<Option<SecretString>> {
        match self.entry(id)?.get_password() {
            Ok(secret) => Ok(Some(SecretString::from(secret))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(KeyringError::Ambiguous(entries)) => {
                // Several items match, e.g. created by an older version in
                // another collection. Use the first one consistently.
                let first = entries.into_iter().next();
                match first.map(|entry| entry.get_password()) {
                    Some(Ok(secret)) => Ok(Some(SecretString::from(secret))),
                    Some(Err(error)) => Err(unavailable(error)),
                    None => Ok(None),
                }
            }
            Err(error) => Err(unavailable(error)),
        }
    }

    fn set(&self, id: &SecretId, secret: &SecretString) -> Result<()> {
        self.entry(id)?
            .set_password(secret.expose_secret())
            .map_err(unavailable)
    }

    fn delete(&self, id: &SecretId) -> Result<()> {
        match self.entry(id)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(unavailable(error)),
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a `map_err` callback"
)]
fn unavailable(error: KeyringError) -> SecretError {
    SecretError::Unavailable(error.to_string())
}
