//! Target files that a transfer is writing, across all transfers of a
//! queue. A file copied twice at the same time (the same selection sent
//! twice, say) would otherwise be written to the same `.ferge-part` by both
//! transfers. The second waits until the first is done with the file and
//! then finds it there, as if it had started afterwards.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::lock;
use crate::path::RemotePath;

/// A file on a target: the server (or this computer) and its path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub(super) server: String,
    pub(super) path: RemotePath,
}

/// The files claimed in a queue.
#[derive(Debug, Default)]
pub(super) struct Claims {
    held: Mutex<HashSet<Key>>,
    released: Notify,
}

impl Claims {
    /// Claims `key`, waiting while someone else holds it.
    pub(super) async fn claim(self: &Arc<Self>, key: Key) -> Claim {
        loop {
            // Registered before looking, so a release in between is not
            // missed.
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if lock(&self.held).insert(key.clone()) {
                return Claim {
                    claims: Arc::clone(self),
                    key,
                };
            }
            released.await;
        }
    }
}

/// A claimed file; released when dropped.
#[derive(Debug)]
pub(super) struct Claim {
    claims: Arc<Claims>,
    key: Key,
}

impl Drop for Claim {
    fn drop(&mut self) {
        lock(&self.claims.held).remove(&self.key);
        self.claims.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn key(server: &str, path: &str) -> Key {
        Key {
            server: server.into(),
            path: RemotePath::parse(path).unwrap(),
        }
    }

    #[tokio::test]
    async fn a_claimed_file_waits_until_it_is_released() {
        let claims = Arc::new(Claims::default());
        let first = claims.claim(key("a", "/x")).await;
        // Other files and the same path elsewhere are free.
        let _other = claims.claim(key("a", "/y")).await;
        let _elsewhere = claims.claim(key("b", "/x")).await;

        let waiting = tokio::spawn({
            let claims = Arc::clone(&claims);
            async move { claims.claim(key("a", "/x")).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        drop(first);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("released")
            .unwrap();
    }
}
