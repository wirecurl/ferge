//! Integration tests against the Docker test server.
//!
//! Start the server with `./testserver/up.sh`, then run
//! `cargo test -p ferry-core --test sftp -- --include-ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use ferry_core::interaction::{HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt};
use ferry_core::remote::sftp::{KnownHostsSource, SftpOptions, SftpRemote, SshConfigSource};
use ferry_core::{EntryKind, EntryName, Error, HostKeyError, Remote, RemotePath};
use futures::{StreamExt as _, TryStreamExt as _};
use secrecy::SecretString;

const MANY_COUNT: usize = 100_000;

fn state_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.testserver")
}

/// Answers prompts from a script and records what was asked.
#[derive(Debug, Default)]
struct Scripted {
    host_key: Option<HostKeyDecision>,
    passphrases: Mutex<Vec<&'static str>>,
    passwords: Mutex<Vec<&'static str>>,
    host_prompts: Mutex<Vec<HostKeyPrompt>>,
    passphrase_retries: Mutex<Vec<bool>>,
}

#[async_trait]
impl Interaction for Scripted {
    async fn confirm_host_key(&self, prompt: HostKeyPrompt) -> HostKeyDecision {
        self.host_prompts.lock().unwrap().push(prompt);
        self.host_key.unwrap_or(HostKeyDecision::Reject)
    }

    async fn key_passphrase(&self, _key_file: &Path, retry: bool) -> Option<SecretString> {
        self.passphrase_retries.lock().unwrap().push(retry);
        let mut queue = self.passphrases.lock().unwrap();
        (!queue.is_empty()).then(|| SecretString::from(queue.remove(0)))
    }

    async fn password(&self, _user: &str, _host: &str, _retry: bool) -> Option<SecretString> {
        let mut queue = self.passwords.lock().unwrap();
        (!queue.is_empty()).then(|| SecretString::from(queue.remove(0)))
    }

    async fn keyboard_interactive(
        &self,
        _name: &str,
        _instructions: &str,
        _prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        None
    }
}

impl Scripted {
    fn accepting() -> Self {
        Self {
            host_key: Some(HostKeyDecision::AcceptAndRemember),
            ..Self::default()
        }
    }
}

/// Options that touch nothing outside the test: no agent, no user config, no
/// default keys, a private known_hosts file.
fn options(known_hosts: &Path) -> SftpOptions {
    let mut options =
        SftpOptions::new(std::env::var("FERRY_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()));
    options.port = Some(
        std::env::var("FERRY_TEST_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(2222),
    );
    options.user = Some("ferry".into());
    options.use_agent = false;
    options.use_default_identities = false;
    options.ssh_config = SshConfigSource::Disabled;
    options.known_hosts = KnownHostsSource::File(known_hosts.to_path_buf());
    options
}

fn key_options(known_hosts: &Path) -> SftpOptions {
    let mut options = options(known_hosts);
    options.identity_files = vec![state_dir().join("id_ed25519")];
    options
}

async fn connect(options: SftpOptions, interaction: Arc<Scripted>) -> Result<SftpRemote, Error> {
    SftpRemote::connect(options, interaction).await
}

fn name(text: &str) -> EntryName {
    EntryName::new(text).expect("valid name")
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn unknown_host_key_is_confirmed_once_then_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");

    let first = Arc::new(Scripted::accepting());
    connect(key_options(&known_hosts), Arc::clone(&first))
        .await
        .expect("first connect");
    let prompts = first.host_prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].host, "[127.0.0.1]:2222");
    assert!(prompts[0].fingerprint.starts_with("SHA256:"));

    let second = Arc::new(Scripted::default());
    connect(key_options(&known_hosts), Arc::clone(&second))
        .await
        .expect("second connect");
    assert!(
        second.host_prompts.lock().unwrap().is_empty(),
        "key must be remembered"
    );
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn rejected_and_changed_host_keys_fail() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    let error = connect(key_options(&known_hosts), Arc::new(Scripted::default()))
        .await
        .expect_err("rejected key");
    assert!(
        matches!(error, Error::HostKey(HostKeyError::Rejected { .. })),
        "{error:?}"
    );

    // Record a different ed25519 key for the server.
    std::fs::write(
        &known_hosts,
        "[127.0.0.1]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJzaHLNcaADvZ+bfJbe+3qF3Z3UkZs0s717aac8buuEz\n",
    )
    .unwrap();
    let error = connect(key_options(&known_hosts), Arc::new(Scripted::accepting()))
        .await
        .expect_err("changed key");
    assert!(
        matches!(error, Error::HostKey(HostKeyError::Changed { line: 1, .. })),
        "{error:?}"
    );
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn password_authentication() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");

    let mut preset = options(&known_hosts);
    preset.password = Some(SecretString::from("ferry"));
    connect(preset, Arc::new(Scripted::accepting()))
        .await
        .expect("preset password");

    let interaction = Arc::new(Scripted {
        passwords: Mutex::new(vec!["wrong", "ferry"]),
        ..Scripted::accepting()
    });
    connect(options(&known_hosts), interaction)
        .await
        .expect("prompted password");

    let error = connect(options(&known_hosts), Arc::new(Scripted::accepting()))
        .await
        .expect_err("no password");
    assert!(error.is_authentication(), "{error:?}");
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn encrypted_key_asks_for_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    let mut options = options(&known_hosts);
    options.identity_files = vec![state_dir().join("id_ed25519_encrypted")];

    let interaction = Arc::new(Scripted {
        passphrases: Mutex::new(vec!["wrong", "ferry"]),
        ..Scripted::accepting()
    });
    connect(options, Arc::clone(&interaction))
        .await
        .expect("connect");
    assert_eq!(
        *interaction.passphrase_retries.lock().unwrap(),
        [false, true]
    );
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn streams_large_listing() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        key_options(&dir.path().join("known_hosts")),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let many = remote.home_dir().await.unwrap().join(&name("many"));

    let started = Instant::now();
    let mut stream = remote.list(&many);
    let first = stream.next().await.expect("first chunk").expect("ok");
    let first_chunk = started.elapsed();
    let mut total = first.len();
    let mut chunks = 1;
    while let Some(chunk) = stream.next().await {
        total += chunk.expect("chunk").len();
        chunks += 1;
    }
    let complete = started.elapsed();
    println!(
        "first chunk after {first_chunk:?}, {total} entries in {chunks} chunks after {complete:?}"
    );
    assert_eq!(total, MANY_COUNT);
    assert!(chunks > 1);
    assert!(
        first_chunk < complete / 4,
        "first rows must arrive long before the end"
    );
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn tree_metadata_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        key_options(&dir.path().join("known_hosts")),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let tree = remote.home_dir().await.unwrap().join(&name("tree"));
    let entries: Vec<_> = remote.list(&tree).try_concat().await.expect("list");
    let kind = |n: &str| {
        entries
            .iter()
            .find(|entry| entry.name.as_str() == n)
            .map_or_else(|| panic!("{n} missing"), ferry_core::Entry::kind)
    };
    assert_eq!(kind("hello.txt"), EntryKind::File);
    assert_eq!(kind("nested"), EntryKind::Directory);
    assert_eq!(kind("link-to-nested"), EntryKind::Symlink);
    assert_eq!(kind("Ünïcödé ✓.txt"), EntryKind::File);
    assert_eq!(kind(".hidden"), EntryKind::File);
    assert!(
        entries
            .iter()
            .all(|e| e.name.as_str() != "." && e.name.as_str() != "..")
    );

    // `stat` follows symlinks; listing a symlinked directory works.
    assert!(
        remote
            .stat(&tree.join(&name("link-to-nested")))
            .await
            .unwrap()
            .is_dir()
    );
    let via_link: Vec<_> = remote
        .list(&tree.join(&name("link-to-nested")))
        .try_concat()
        .await
        .unwrap();
    assert_eq!(via_link.len(), 1);

    let missing = tree.join(&name("missing"));
    let error = remote
        .list(&missing)
        .try_concat()
        .await
        .expect_err("missing dir");
    assert!(matches!(error, Error::NotFound { .. }), "{error:?}");
    assert!(matches!(
        remote.stat(&missing).await,
        Err(Error::NotFound { .. })
    ));
    assert_eq!(
        remote
            .canonicalize(&RemotePath::parse("/home/ferry/tree/link-to-nested").unwrap())
            .await
            .unwrap(),
        RemotePath::parse("/home/ferry/tree/nested").unwrap()
    );
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn file_operations_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        key_options(&dir.path().join("known_hosts")),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let unique = format!(
        "scratch-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    );
    let scratch = remote.home_dir().await.unwrap().join(&name(&unique));
    remote.mkdir(&scratch).await.expect("mkdir");

    let file = scratch.join(&name("data.bin"));
    let payload: Vec<u8> = (0..3 * 1024 * 1024_u32).map(|i| (i % 251) as u8).collect();
    let chunks: Vec<Result<Bytes, Error>> = payload
        .chunks(100_000)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    let written = remote
        .write(&file, futures::stream::iter(chunks).boxed())
        .await
        .expect("write");
    assert_eq!(written, payload.len() as u64);
    let read: Vec<Bytes> = remote
        .read(&file)
        .await
        .expect("open")
        .try_collect()
        .await
        .expect("read");
    assert_eq!(read.concat(), payload);

    remote.chmod(&file, 0o640).await.expect("chmod");
    let metadata = remote.stat(&file).await.expect("stat");
    assert_eq!(metadata.permissions, Some(0o640));
    assert_eq!(metadata.size, payload.len() as u64);

    let sub = scratch.join(&name("sub"));
    remote.mkdir(&sub).await.expect("mkdir sub");
    assert!(matches!(
        remote.rename(&file, &sub).await,
        Err(Error::AlreadyExists { .. })
    ));
    let renamed = scratch.join(&name("renamed.bin"));
    remote.rename(&file, &renamed).await.expect("rename");

    remote.delete_file(&renamed).await.expect("delete file");
    remote.delete_dir(&sub).await.expect("delete sub");
    remote.delete_dir(&scratch).await.expect("delete scratch");
    remote.disconnect().await;
}
