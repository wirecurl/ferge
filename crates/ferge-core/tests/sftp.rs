//! Integration tests against the Docker test server.
//!
//! Start the server with `./testserver/up.sh`, then run
//! `cargo test -p ferge-core --test sftp -- --include-ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use ferge_core::interaction::{
    Account, HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt,
};
use ferge_core::remote::sftp::{KnownHostsSource, SftpOptions, SftpRemote, SshConfigSource};
use ferge_core::{ElevationError, EntryKind, EntryName, Error, HostKeyError, Remote, RemotePath};
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
    sudo_passwords: Mutex<Vec<&'static str>>,
    sudo_retries: Mutex<Vec<bool>>,
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

    async fn password(&self, _account: &Account, _retry: bool) -> Option<SecretString> {
        let mut queue = self.passwords.lock().unwrap();
        (!queue.is_empty()).then(|| SecretString::from(queue.remove(0)))
    }

    async fn keyboard_interactive(
        &self,
        _account: &Account,
        _name: &str,
        _instructions: &str,
        _prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        None
    }

    async fn sudo_password(&self, _account: &Account, retry: bool) -> Option<SecretString> {
        self.sudo_retries.lock().unwrap().push(retry);
        let mut queue = self.sudo_passwords.lock().unwrap();
        (!queue.is_empty()).then(|| SecretString::from(queue.remove(0)))
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
        SftpOptions::new(std::env::var("FERGE_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()));
    options.port = Some(
        std::env::var("FERGE_TEST_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(2222),
    );
    options.user = Some("ferge".into());
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

fn user_options(known_hosts: &Path, user: &str) -> SftpOptions {
    let mut options = key_options(known_hosts);
    options.user = Some(user.into());
    options
}

fn path(text: &str) -> RemotePath {
    RemotePath::parse(text).expect("valid path")
}

async fn read_all(remote: &SftpRemote, file: &RemotePath) -> Vec<u8> {
    let chunks: Vec<Bytes> = remote
        .read(file)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    chunks.concat()
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
    preset.password = Some(SecretString::from("ferge"));
    connect(preset, Arc::new(Scripted::accepting()))
        .await
        .expect("preset password");

    let interaction = Arc::new(Scripted {
        passwords: Mutex::new(vec!["wrong", "ferge"]),
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
        passphrases: Mutex::new(vec!["wrong", "ferge"]),
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
async fn writers_never_follow_server_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        key_options(&dir.path().join("known_hosts")),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let tree = remote.home_dir().await.unwrap().join(&name("tree"));
    let hello = tree.join(&name("hello.txt"));
    let before = remote.stat(&hello).await.unwrap().size;
    for link in ["link-to-hello", "dangling-link"] {
        let link = tree.join(&name(link));
        assert!(remote.open_writer(&link, 0).await.is_err(), "{link}");
        assert!(remote.open_writer(&link, 1).await.is_err(), "{link}");
        assert_eq!(remote.lstat(&link).await.unwrap().kind, EntryKind::Symlink);
    }
    assert_eq!(remote.stat(&hello).await.unwrap().size, before);
    assert!(matches!(
        remote.stat(&tree.join(&name("does-not-exist"))).await,
        Err(Error::NotFound { .. })
    ));
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
            .map_or_else(|| panic!("{n} missing"), ferge_core::Entry::kind)
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
            .canonicalize(&RemotePath::parse("/home/ferge/tree/link-to-nested").unwrap())
            .await
            .unwrap(),
        RemotePath::parse("/home/ferge/tree/nested").unwrap()
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

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn resumes_reads_and_writes_and_replaces_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        key_options(&dir.path().join("known_hosts")),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let unique = format!("resume-{}", std::process::id());
    let scratch = remote.home_dir().await.unwrap().join(&name(&unique));
    remote.mkdir(&scratch).await.expect("mkdir");

    // An odd size, so the last chunk is short and the window does not divide it.
    let payload: Vec<u8> = (0..5 * 1024 * 1024 + 4_321_u32)
        .map(|i| (i.wrapping_mul(31) % 253) as u8)
        .collect();
    let file = scratch.join(&name("big.bin"));
    let split = 2 * 1024 * 1024 + 17;

    // Write the first part, then resume at `split` with the rest.
    let mut writer = remote.open_writer(&file, 0).await.expect("open");
    writer
        .write(Bytes::copy_from_slice(&payload[..split + 1000]))
        .await
        .expect("write");
    assert_eq!(
        writer.finish().await.expect("finish"),
        (split + 1000) as u64
    );
    let mut writer = remote
        .open_writer(&file, split as u64)
        .await
        .expect("reopen");
    for chunk in payload[split..].chunks(700_000) {
        writer
            .write(Bytes::copy_from_slice(chunk))
            .await
            .expect("write");
    }
    assert_eq!(writer.finish().await.expect("finish"), payload.len() as u64);
    assert_eq!(remote.stat(&file).await.unwrap().size, payload.len() as u64);

    let whole: Vec<Bytes> = remote
        .read(&file)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(whole.concat(), payload);
    let tail: Vec<Bytes> = remote
        .read_from(&file, split as u64)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(tail.concat(), payload[split..]);
    let empty: Vec<Bytes> = remote
        .read_from(&file, payload.len() as u64)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(empty.concat(), b"");

    // Replace an existing file and keep a modification time.
    let other = scratch.join(&name("other.bin"));
    remote
        .write(
            &other,
            futures::stream::iter([Ok(Bytes::from_static(b"old"))]).boxed(),
        )
        .await
        .unwrap();
    remote.rename_replace(&file, &other).await.expect("replace");
    assert_eq!(
        remote.stat(&other).await.unwrap().size,
        payload.len() as u64
    );
    assert!(matches!(
        remote.stat(&file).await,
        Err(Error::NotFound { .. })
    ));
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    remote.set_modified(&other, when).await.expect("set mtime");
    assert_eq!(remote.stat(&other).await.unwrap().modified, Some(when));

    remote.delete_file(&other).await.unwrap();
    remote.delete_dir(&scratch).await.unwrap();
    remote.disconnect().await;
}

async fn settled(
    queue: &ferge_core::transfer::TransferQueue,
    id: ferge_core::transfer::TransferId,
) -> ferge_core::transfer::TransferSnapshot {
    loop {
        let snapshot = queue
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .unwrap();
        if snapshot.state.is_settled() {
            return snapshot;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn uploads_and_downloads_a_tree() {
    use ferge_core::remote::local::LocalRemote;
    use ferge_core::transfer::{
        ConflictAction, FixedPolicy, TransferQueue, TransferRequest, TransferState,
    };

    let dir = tempfile::tempdir().unwrap();
    let remote: Arc<dyn Remote> = Arc::new(
        connect(
            key_options(&dir.path().join("known_hosts")),
            Arc::new(Scripted::accepting()),
        )
        .await
        .expect("connect"),
    );
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let local_path = |path: &Path| RemotePath::parse(path.to_str().unwrap()).unwrap();

    let source = dir.path().join("upload");
    std::fs::create_dir_all(source.join("nested/deeper")).unwrap();
    let big: Vec<u8> = (0..4 * 1024 * 1024 + 99_u32)
        .map(|i| (i % 241) as u8)
        .collect();
    std::fs::write(source.join("big.bin"), &big).unwrap();
    std::fs::write(source.join("nested/small.txt"), b"small").unwrap();
    std::fs::write(source.join("nested/deeper/empty"), b"").unwrap();

    let scratch = remote
        .home_dir()
        .await
        .unwrap()
        .join(&name(&format!("transfer-{}", std::process::id())));
    remote.mkdir(&scratch).await.unwrap();

    let (queue, _events) = TransferQueue::new(4);
    let up = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&local),
            items: vec![local_path(&source)],
            target: Arc::clone(&remote),
            target_dir: scratch.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let uploaded = settled(&queue, up).await;
    assert_eq!(
        uploaded.state,
        TransferState::Completed,
        "{:?}",
        uploaded.errors
    );
    assert_eq!(uploaded.files_done, 3);

    let back = dir.path().join("back");
    std::fs::create_dir(&back).unwrap();
    let down = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&remote),
            items: vec![scratch.join(&name("upload"))],
            target: Arc::clone(&local),
            target_dir: local_path(&back),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let downloaded = settled(&queue, down).await;
    assert_eq!(
        downloaded.state,
        TransferState::Completed,
        "{:?}",
        downloaded.errors
    );
    assert_eq!(std::fs::read(back.join("upload/big.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read(back.join("upload/nested/small.txt")).unwrap(),
        b"small"
    );
    assert_eq!(
        std::fs::read(back.join("upload/nested/deeper/empty")).unwrap(),
        b""
    );
    // Times survive the round trip (SFTP keeps whole seconds).
    let seconds = |path: &Path| {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    assert_eq!(
        seconds(&back.join("upload/big.bin")),
        seconds(&source.join("big.bin"))
    );

    // Clean up on the server: the whole tree in one call.
    let deleted = ferge_core::ops::delete(
        remote.as_ref(),
        &[(
            scratch.join(&name("upload")),
            ferge_core::EntryKind::Directory,
        )],
        &|_| {},
    )
    .await
    .expect("delete tree");
    assert_eq!(deleted, 6);
    remote.delete_dir(&scratch).await.unwrap();
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn works_as_root_with_the_sudo_password() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        key_options(&dir.path().join("known_hosts")),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let private = path("/root/private");
    let denied = remote
        .list(&private)
        .try_concat()
        .await
        .expect_err("denied");
    assert!(denied.is_permission_denied(), "{denied:?}");

    // A wrong password is asked again; the right one opens the session.
    let interaction = Arc::new(Scripted::accepting());
    interaction
        .sudo_passwords
        .lock()
        .unwrap()
        .extend(["wrong", "ferge"]);
    let root = remote
        .elevate(Arc::clone(&interaction) as Arc<dyn Interaction>)
        .await
        .expect("elevate");
    assert_eq!(*interaction.sudo_retries.lock().unwrap(), [false, true]);
    assert!(root.is_elevated() && !remote.is_elevated());
    assert_eq!(root.label(), "root@127.0.0.1:2222");
    assert_eq!(root.account(), remote.account());
    assert_eq!(root.home_dir().await.unwrap(), path("/home/ferge"));

    let entries: Vec<_> = root
        .list(&private)
        .try_concat()
        .await
        .expect("list as root");
    assert!(
        entries
            .iter()
            .any(|entry| entry.name.as_str() == "secret.txt")
    );
    assert_eq!(
        read_all(&root, &private.join(&name("secret.txt"))).await,
        b"only root can read this\n"
    );
    let written = private.join(&name("written-as-root.txt"));
    let data = futures::stream::iter([Ok(Bytes::from_static(b"root was here"))]).boxed();
    assert_eq!(root.write(&written, data).await.unwrap(), 13);
    assert_eq!(read_all(&root, &written).await, b"root was here");
    root.delete_file(&written).await.unwrap();

    // Leaving root closes only its channels; the login session goes on.
    root.disconnect().await;
    drop(root);
    let home: Vec<_> = remote
        .list(&path("/home/ferge"))
        .try_concat()
        .await
        .unwrap();
    assert!(home.iter().any(|entry| entry.name.as_str() == "tree"));
    remote.disconnect().await;
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn works_as_root_without_asking_when_sudo_needs_no_password() {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        user_options(&dir.path().join("known_hosts"), "ferge-nopasswd"),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let interaction = Arc::new(Scripted::accepting());
    let root = remote
        .elevate(Arc::clone(&interaction) as Arc<dyn Interaction>)
        .await
        .expect("elevate");
    assert!(interaction.sudo_retries.lock().unwrap().is_empty());
    let entries: Vec<_> = root
        .list(&path("/root/private"))
        .try_concat()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn asks_sudo_about_sftp_server_itself() {
    // sudo needs a password for everything but sftp-server. A password sent
    // anyway would reach sftp-server's input; none may be asked for.
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        user_options(&dir.path().join("known_hosts"), "ferge-sftponly"),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let interaction = Arc::new(Scripted::accepting());
    let root = remote
        .elevate(Arc::clone(&interaction) as Arc<dyn Interaction>)
        .await
        .expect("elevate without a password");
    assert!(interaction.sudo_retries.lock().unwrap().is_empty());
    let entries: Vec<_> = root
        .list(&path("/root/private"))
        .try_concat()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
#[ignore = "requires the test server (./testserver/up.sh)"]
async fn working_as_root_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");

    // Not in the sudoers file: sudo's own explanation is passed on.
    let remote = connect(
        user_options(&known_hosts, "ferge-nosudo"),
        Arc::new(Scripted::accepting()),
    )
    .await
    .expect("connect");
    let interaction = Arc::new(Scripted::accepting());
    interaction.sudo_passwords.lock().unwrap().push("ferge");
    let error = remote
        .elevate(interaction as Arc<dyn Interaction>)
        .await
        .expect_err("not allowed");
    assert!(
        matches!(
            &error,
            Error::Elevation(ElevationError::Refused { message, .. })
                if message.contains("not in the sudoers file")
        ),
        "{error:?}"
    );

    // Closing the password dialog cancels quietly.
    let remote = connect(key_options(&known_hosts), Arc::new(Scripted::accepting()))
        .await
        .expect("connect");
    let error = remote
        .elevate(Arc::new(Scripted::accepting()) as Arc<dyn Interaction>)
        .await
        .expect_err("cancelled");
    assert!(error.is_cancelled(), "{error:?}");
}
