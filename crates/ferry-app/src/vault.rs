//! Access to saved passwords for the whole app.
//!
//! The store is opened lazily on the I/O runtime (connecting to the Secret
//! Service takes a D-Bus round trip, which must not delay the first frame).
//! When the store is the encrypted file, the vault asks for the master
//! password the first time a secret is needed, once for all tabs.

use std::sync::Arc;

use ferry_core::secrets::{self, MemoryStore, SecretError, SecretId, SecretStore};
use secrecy::SecretString;
use tokio::sync::{Mutex, OnceCell};

use crate::prompts::Prompter;

/// Wrong master passwords accepted in a row before giving up.
const MAX_UNLOCK_ATTEMPTS: usize = 3;

/// Saved passwords and passphrases.
#[derive(Debug)]
pub(crate) struct Vault {
    store: OnceCell<Arc<dyn SecretStore>>,
    /// Keeps the in-memory store for runs that must not touch the user's data.
    ephemeral: bool,
    /// Serializes unlock prompts; remembers a declined unlock so the user is
    /// not asked again for every connection.
    unlock: Mutex<UnlockState>,
}

#[derive(Debug, Default)]
struct UnlockState {
    declined: bool,
}

impl Vault {
    /// The platform's store, or the encrypted file.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            store: OnceCell::new(),
            ephemeral: false,
            unlock: Mutex::default(),
        })
    }

    /// A vault that forgets everything on exit (benchmarks, tests).
    pub(crate) fn ephemeral() -> Arc<Self> {
        Arc::new(Self {
            store: OnceCell::new(),
            ephemeral: true,
            unlock: Mutex::default(),
        })
    }

    /// Opens the store if that has not happened yet. Must run on the runtime.
    pub(crate) async fn store(&self) -> Arc<dyn SecretStore> {
        let ephemeral = self.ephemeral;
        self.store
            .get_or_init(|| async move {
                if ephemeral {
                    return Arc::new(MemoryStore::default()) as Arc<dyn SecretStore>;
                }
                let opened =
                    tokio::task::spawn_blocking(|| match secrets::default_fallback_path() {
                        Some(path) => secrets::open_default(&path),
                        None => Arc::new(MemoryStore::default()),
                    })
                    .await;
                opened.unwrap_or_else(|error| {
                    tracing::error!(%error, "opening the password store failed");
                    Arc::new(MemoryStore::default())
                })
            })
            .await
            .clone()
    }

    /// The saved secret, if there is one and the store could be opened.
    pub(crate) async fn lookup(&self, id: &SecretId, prompter: &Prompter) -> Option<SecretString> {
        let store = self.store().await;
        if !self.ensure_unlocked(&store, prompter, false).await {
            return None;
        }
        let key = id.clone();
        let lookup = tokio::task::spawn_blocking(move || store.get(&key)).await;
        match lookup {
            Ok(Ok(secret)) => secret,
            Ok(Err(error)) => {
                tracing::warn!(%error, %id, "reading a saved secret failed");
                None
            }
            Err(_) => None,
        }
    }

    /// Saves `secret`, creating or unlocking the password file if needed.
    pub(crate) async fn save(&self, id: SecretId, secret: SecretString, prompter: &Prompter) {
        let store = self.store().await;
        if !self.ensure_unlocked(&store, prompter, true).await {
            return;
        }
        let label = id.to_string();
        let saved = tokio::task::spawn_blocking(move || store.set(&id, &secret)).await;
        if let Ok(Err(error)) = saved {
            tracing::warn!(%error, "saving a secret failed");
            prompter.notice(format!("Could not save the {label}: {error}"));
        }
    }

    /// Removes a saved secret, e.g. one the server no longer accepts.
    pub(crate) async fn forget(&self, id: SecretId, prompter: &Prompter) {
        let store = self.store().await;
        if !self.ensure_unlocked(&store, prompter, false).await {
            return;
        }
        let forgotten = tokio::task::spawn_blocking(move || store.delete(&id)).await;
        if let Ok(Err(error)) = forgotten {
            tracing::warn!(%error, "removing a saved secret failed");
        }
    }

    /// Makes sure the store can be used. For the encrypted file this asks for
    /// the master password; `for_saving` also creates a new file.
    async fn ensure_unlocked(
        &self,
        store: &Arc<dyn SecretStore>,
        prompter: &Prompter,
        for_saving: bool,
    ) -> bool {
        let Some(file) = store.as_file_store() else {
            return true;
        };
        if file.is_unlocked() {
            return true;
        }
        let mut state = self.unlock.lock().await;
        // Another connection may have unlocked it while this one waited.
        if file.is_unlocked() {
            return true;
        }
        let exists = file.exists();
        if !exists && !for_saving {
            return false; // nothing saved yet
        }
        if state.declined && !for_saving {
            return false;
        }
        for attempt in 0..MAX_UNLOCK_ATTEMPTS {
            let Some(master) = prompter.master_password(!exists, attempt > 0).await else {
                state.declined = true;
                return false;
            };
            let store = Arc::clone(store);
            let unlocked = tokio::task::spawn_blocking(move || {
                store
                    .as_file_store()
                    .map_or(Ok(()), |file| file.unlock(&master))
            })
            .await;
            match unlocked {
                Ok(Ok(())) => {
                    state.declined = false;
                    return true;
                }
                Ok(Err(SecretError::WrongPassword)) => {}
                Ok(Err(error)) => {
                    prompter.notice(format!("Could not open the saved passwords: {error}"));
                    return false;
                }
                Err(_) => return false,
            }
        }
        state.declined = true;
        false
    }
}
