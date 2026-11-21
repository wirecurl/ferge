//! Round trip through the platform's credential store.
//!
//! Ignored by default: it needs a running Secret Service (Linux) or an
//! unlocked login Keychain (macOS), and briefly stores a dummy entry there.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use ferge_core::secrets::{KeyringStore, SecretId, SecretStore as _};
use secrecy::{ExposeSecret as _, SecretString};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a platform credential store"]
async fn keyring_round_trip() {
    tokio::task::spawn_blocking(|| {
        let store = KeyringStore::open().expect("a credential store");
        let id = SecretId::Password {
            user: "ferge-test".into(),
            host: "keyring-test.invalid".into(),
            port: 22,
        };
        store.delete(&id).unwrap();
        assert!(store.get(&id).unwrap().is_none());
        store.set(&id, &SecretString::from("first")).unwrap();
        store.set(&id, &SecretString::from("sëcond")).unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().expose_secret(), "sëcond");
        store.delete(&id).unwrap();
        assert!(store.get(&id).unwrap().is_none());
    })
    .await
    .unwrap();
}
