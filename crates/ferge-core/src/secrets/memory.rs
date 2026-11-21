//! A store that forgets everything when dropped, for tests and for users who
//! do not want anything saved.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use secrecy::SecretString;

use super::{Result, SecretId, SecretStore};

/// Keeps secrets in memory only.
#[derive(Debug, Default)]
pub struct MemoryStore {
    secrets: Mutex<HashMap<SecretId, SecretString>>,
}

impl SecretStore for MemoryStore {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn get(&self, id: &SecretId) -> Result<Option<SecretString>> {
        let secrets = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(secrets.get(id).cloned())
    }

    fn set(&self, id: &SecretId, secret: &SecretString) -> Result<()> {
        let mut secrets = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
        secrets.insert(id.clone(), secret.clone());
        Ok(())
    }

    fn delete(&self, id: &SecretId) -> Result<()> {
        let mut secrets = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
        secrets.remove(id);
        Ok(())
    }
}
