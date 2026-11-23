//! Integration tests against the FTP test server (vsftpd).
//!
//! Start it with `./testserver/up.sh`, then run
//! `cargo test -p ferge-core --test ftp -- --include-ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use ferge_core::destination::FtpTls;
use ferge_core::interaction::{
    Account, CertificatePrompt, HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt,
    PlaintextPrompt,
};
use ferge_core::remote::ftp::{FtpOptions, FtpRemote};
use ferge_core::remote::local::LocalRemote;
use ferge_core::tls::{Fingerprint, KnownCertificates, SystemTrust, TlsOptions};
use ferge_core::transfer::{
    ConflictAction, FixedPolicy, TransferQueue, TransferRequest, TransferState,
};
use ferge_core::{CertificateError, EntryKind, Error, FtpTarget, Remote, RemotePath};
use futures::{StreamExt as _, TryStreamExt as _};
use secrecy::SecretString;

const MANY_COUNT: usize = 100_000;

fn port(variable: &str, default: u16) -> u16 {
    std::env::var(variable)
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(default)
}

fn state_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.testserver")
}

/// The test server's certificate, as `up.sh` saved it.
fn server_fingerprint() -> String {
    let pem = std::fs::read_to_string(state_dir().join("ftp-cert.pem")).unwrap();
    let base64: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = <base64ct::Base64 as base64ct::Encoding>::decode_vec(&base64).unwrap();
    Fingerprint::of(&der).to_string()
}

/// Answers from a script and keeps what was asked.
#[derive(Debug)]
struct Scripted {
    certificate: HostKeyDecision,
    plaintext: HostKeyDecision,
    passwords: Mutex<Vec<&'static str>>,
    password_retries: Mutex<Vec<bool>>,
    certificates: Mutex<Vec<CertificatePrompt>>,
    plaintext_asked: AtomicUsize,
}

impl Scripted {
    fn new(passwords: &[&'static str]) -> Arc<Self> {
        Arc::new(Self {
            certificate: HostKeyDecision::AcceptOnce,
            plaintext: HostKeyDecision::Reject,
            passwords: Mutex::new(passwords.to_vec()),
            password_retries: Mutex::new(Vec::new()),
            certificates: Mutex::new(Vec::new()),
            plaintext_asked: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Interaction for Scripted {
    async fn confirm_host_key(&self, _prompt: HostKeyPrompt) -> HostKeyDecision {
        HostKeyDecision::Reject
    }
    async fn key_passphrase(&self, _key_file: &Path, _retry: bool) -> Option<SecretString> {
        None
    }
    async fn password(&self, account: &Account, retry: bool) -> Option<SecretString> {
        assert_eq!(account.user, "ferge");
        self.password_retries.lock().unwrap().push(retry);
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
    async fn confirm_certificate(&self, prompt: CertificatePrompt) -> HostKeyDecision {
        self.certificates.lock().unwrap().push(prompt);
        self.certificate
    }
    async fn confirm_plaintext(&self, _prompt: PlaintextPrompt) -> HostKeyDecision {
        self.plaintext_asked.fetch_add(1, Ordering::SeqCst);
        self.plaintext
    }
}

fn options(tls: FtpTls, port: u16, known: &Path) -> FtpOptions {
    let host = std::env::var("FERGE_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut target = FtpTarget::new(&host, tls).unwrap();
    target.port = Some(port);
    target.user = Some("ferge".into());
    let mut options = FtpOptions::new(&target);
    options.tls_options = TlsOptions {
        system: SystemTrust::Platform,
        known: Some(KnownCertificates::at(known)),
    };
    options
}

fn explicit(known: &Path) -> FtpOptions {
    options(FtpTls::Explicit, port("FERGE_TEST_FTP_PORT", 2121), known)
}

async fn connect(options: FtpOptions, interaction: &Arc<Scripted>) -> Result<FtpRemote, Error> {
    FtpRemote::connect(options, Arc::clone(interaction) as Arc<dyn Interaction>).await
}

/// A logged-in connection over explicit TLS.
async fn session() -> (FtpRemote, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let remote = connect(
        explicit(&dir.path().join("known")),
        &Scripted::new(&["ferge"]),
    )
    .await
    .unwrap();
    (remote, dir)
}

fn path(text: &str) -> RemotePath {
    RemotePath::parse(text).unwrap()
}

/// A folder of its own below the login folder, removed by `cleanup`.
async fn scratch(remote: &FtpRemote, name: &str) -> RemotePath {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = path(&format!("/home/ferge/ftp-test-{name}-{stamp}"));
    remote.mkdir(&dir).await.unwrap();
    dir
}

async fn cleanup(remote: &dyn Remote, dir: &RemotePath) {
    let entries: Vec<_> = remote
        .list(dir)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .concat();
    for entry in entries {
        let child = dir.join(&entry.name);
        if entry.kind() == EntryKind::Directory {
            Box::pin(cleanup(remote, &child)).await;
        } else {
            remote.delete_file(&child).await.unwrap();
        }
    }
    remote.delete_dir(dir).await.unwrap();
}

async fn read_all(remote: &dyn Remote, file: &RemotePath, offset: u64) -> Vec<u8> {
    let chunks: Vec<Bytes> = remote
        .read_from(file, offset)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    chunks.concat()
}

async fn names(remote: &dyn Remote, dir: &RemotePath) -> Vec<(String, EntryKind)> {
    let mut names: Vec<_> = remote
        .list(dir)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .concat()
        .into_iter()
        .map(|entry| (entry.name.as_str().to_owned(), entry.kind()))
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn asks_about_the_certificate_and_lists_with_any_names() {
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known");
    let interaction = Scripted::new(&["ferge"]);
    let remote = connect(explicit(&known), &interaction).await.unwrap();
    let prompts = interaction.certificates.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].fingerprint, server_fingerprint());
    assert!(prompts[0].subject.contains("ferge-test"), "{prompts:?}");
    assert!(prompts[0].names.contains(&"127.0.0.1".to_owned()));
    assert_eq!(*interaction.password_retries.lock().unwrap(), [false]);
    assert_eq!(remote.home_dir().await.unwrap().as_str(), "/home/ferge");
    assert!(remote.label().starts_with("ferge@127.0.0.1"));

    let tree = names(&remote, &path("/home/ferge/tree")).await;
    for expected in [
        (".hidden", EntryKind::File),
        ("Ünïcödé ✓.txt", EntryKind::File),
        ("with space.txt", EntryKind::File),
        ("nested", EntryKind::Directory),
        ("link-to-hello", EntryKind::Symlink),
        ("dangling-link", EntryKind::Symlink),
    ] {
        assert!(
            tree.contains(&(expected.0.to_owned(), expected.1)),
            "{expected:?} in {tree:?}"
        );
    }
    assert_eq!(
        read_all(&remote, &path("/home/ferge/tree/Ünïcödé ✓.txt"), 0)
            .await
            .len(),
        13
    );
    // Folders whose names look like glob patterns list as themselves.
    let scratch = scratch(&remote, "glob").await;
    let tricky = scratch.join(&ferge_core::EntryName::new("[a]*?").unwrap());
    remote.mkdir(&tricky).await.unwrap();
    remote
        .write(
            &tricky.join(&ferge_core::EntryName::new("inside").unwrap()),
            futures::stream::iter([Ok(Bytes::from_static(b"x"))]).boxed(),
        )
        .await
        .unwrap();
    assert_eq!(
        names(&remote, &tricky).await,
        [("inside".to_owned(), EntryKind::File)]
    );
    cleanup(&remote, &scratch).await;

    // Remembering it: no question the next time.
    let remember = Arc::new(Scripted {
        certificate: HostKeyDecision::AcceptAndRemember,
        ..Arc::into_inner(Scripted::new(&["ferge"])).unwrap()
    });
    connect(explicit(&known), &remember).await.unwrap();
    let again = Scripted::new(&["ferge"]);
    let again_options = FtpOptions {
        password: Some(SecretString::from("ferge")),
        ..explicit(&known)
    };
    connect(again_options, &again).await.unwrap();
    assert!(again.certificates.lock().unwrap().is_empty());
    assert!(
        again.password_retries.lock().unwrap().is_empty(),
        "the given password is tried first"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn streams_a_large_folder() {
    let (remote, _dir) = session().await;
    let started = Instant::now();
    let mut stream = remote.list(&path("/home/ferge/many"));
    let first = stream.next().await.unwrap().unwrap();
    let first_after = started.elapsed();
    assert!(!first.is_empty() && first.len() <= 256, "{}", first.len());
    let mut total = first.len();
    let mut chunks = 1;
    while let Some(chunk) = stream.next().await {
        total += chunk.unwrap().len();
        chunks += 1;
    }
    let all_after = started.elapsed();
    assert_eq!(total, MANY_COUNT);
    assert!(chunks > 2, "{chunks}");
    // vsftpd reads and sorts the whole folder before it sends the first
    // line, so when the first rows come is up to the server; what follows
    // is up to ferge.
    println!("first rows after {first_after:?}, all after {all_after:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn implicit_tls_works_too() {
    let dir = tempfile::tempdir().unwrap();
    let options = options(
        FtpTls::Implicit,
        port("FERGE_TEST_FTPS_PORT", 2990),
        &dir.path().join("known"),
    );
    let interaction = Scripted::new(&["ferge"]);
    let remote = connect(options, &interaction).await.unwrap();
    assert_eq!(interaction.certificates.lock().unwrap().len(), 1);
    assert_eq!(
        read_all(&remote, &path("/home/ferge/tree/hello.txt"), 0)
            .await
            .len(),
        34
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn a_declined_certificate_or_password_fails_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known");
    let decline = Arc::new(Scripted {
        certificate: HostKeyDecision::Reject,
        ..Arc::into_inner(Scripted::new(&["ferge"])).unwrap()
    });
    let error = connect(explicit(&known), &decline).await.unwrap_err();
    assert!(
        matches!(error, Error::Certificate(CertificateError::Rejected { .. })),
        "{error:?}"
    );
    assert!(
        decline.password_retries.lock().unwrap().is_empty(),
        "no password sent"
    );

    let wrong = Scripted::new(&["wrong", "ferge"]);
    connect(explicit(&known), &wrong).await.unwrap();
    assert_eq!(*wrong.password_retries.lock().unwrap(), [false, true]);

    let none = Scripted::new(&[]);
    let error = connect(explicit(&known), &none).await.unwrap_err();
    assert!(error.is_authentication(), "{error:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn asks_before_logging_in_without_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known");
    let plain = || {
        options(
            FtpTls::Explicit,
            port("FERGE_TEST_FTP_PLAIN_PORT", 2122),
            &known,
        )
    };
    let refuse = Scripted::new(&["ferge"]);
    let error = connect(plain(), &refuse).await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::Certificate(CertificateError::PlaintextRefused { .. })
        ),
        "{error:?}"
    );
    assert!(
        refuse.password_retries.lock().unwrap().is_empty(),
        "the password is not sent in plain text"
    );

    let allow = Arc::new(Scripted {
        plaintext: HostKeyDecision::AcceptAndRemember,
        ..Arc::into_inner(Scripted::new(&["ferge", "ferge"])).unwrap()
    });
    let remote = connect(plain(), &allow).await.unwrap();
    assert_eq!(
        read_all(&remote, &path("/home/ferge/tree/hello.txt"), 0)
            .await
            .len(),
        34
    );
    connect(plain(), &allow).await.unwrap();
    assert_eq!(
        allow.plaintext_asked.load(Ordering::SeqCst),
        1,
        "remembered"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn reads_writes_and_changes_files() {
    let (remote, _dir) = session().await;
    let dir = scratch(&remote, "ops").await;
    let name = |name: &str| dir.join(&ferge_core::EntryName::new(name).unwrap());
    let data: Vec<u8> = (0..3_000_000_u32).map(|i| (i % 251) as u8).collect();

    // Writing, reading from an offset.
    let file = name("data.bin");
    let written = remote
        .write(
            &file,
            futures::stream::iter(
                data.chunks(100_000)
                    .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                    .collect::<Vec<_>>(),
            )
            .boxed(),
        )
        .await
        .unwrap();
    assert_eq!(written, data.len() as u64);
    assert_eq!(read_all(&remote, &file, 0).await, data);
    assert_eq!(
        read_all(&remote, &file, 1_000_000).await,
        &data[1_000_000..]
    );
    let metadata = remote.stat(&file).await.unwrap();
    assert_eq!(
        (metadata.kind, metadata.size),
        (EntryKind::File, data.len() as u64)
    );
    assert!(matches!(
        remote.open_writer(&file, 0).await.err(),
        Some(Error::AlreadyExists { .. })
    ));

    // Resuming a write keeps the start and continues.
    let mut writer = remote.open_writer(&name("half.bin"), 0).await.unwrap();
    writer
        .write(Bytes::copy_from_slice(&data[..1_000_000]))
        .await
        .unwrap();
    writer.finish().await.unwrap();
    let mut writer = remote
        .open_writer(&name("half.bin"), 1_000_000)
        .await
        .unwrap();
    writer
        .write(Bytes::copy_from_slice(&data[1_000_000..]))
        .await
        .unwrap();
    assert_eq!(writer.finish().await.unwrap(), data.len() as u64);
    assert_eq!(read_all(&remote, &name("half.bin"), 0).await, data);

    // Times.
    let old = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    remote.set_modified(&file, old).await.unwrap();
    assert_eq!(remote.stat(&file).await.unwrap().modified, Some(old));

    // Renaming never replaces, unless asked to.
    let other = name("other.bin");
    remote
        .write(
            &other,
            futures::stream::iter([Ok(Bytes::from_static(b"other"))]).boxed(),
        )
        .await
        .unwrap();
    assert!(matches!(
        remote.rename(&file, &other).await,
        Err(Error::AlreadyExists { .. })
    ));
    remote.rename_replace(&file, &other).await.unwrap();
    assert_eq!(read_all(&remote, &other, 0).await, data);
    assert!(matches!(
        remote.stat(&file).await,
        Err(Error::NotFound { .. })
    ));
    remote.rename(&other, &file).await.unwrap();

    cleanup(&remote, &dir).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn makes_folders_and_tells_what_is_missing() {
    let (remote, _dir) = session().await;
    let dir = scratch(&remote, "folders").await;
    let name = |name: &str| dir.join(&ferge_core::EntryName::new(name).unwrap());
    let file = name("data.bin");
    remote
        .write(
            &file,
            futures::stream::iter([Ok(Bytes::from_static(b"data"))]).boxed(),
        )
        .await
        .unwrap();
    let sub = name("sub");
    remote.mkdir(&sub).await.unwrap();
    assert!(matches!(
        remote.mkdir(&sub).await,
        Err(Error::AlreadyExists { .. })
    ));
    assert!(remote.stat(&sub).await.unwrap().is_dir());
    remote.chmod(&file, 0o600).await.unwrap();
    let listed = remote
        .list(&dir)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .concat();
    let entry = listed
        .iter()
        .find(|entry| entry.name.as_str() == "data.bin")
        .unwrap();
    assert_eq!(entry.metadata.permissions, Some(0o600));
    assert!(matches!(
        remote.stat(&name("missing")).await,
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        remote.delete_file(&name("missing")).await,
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        remote.list(&name("missing")).next().await,
        Some(Err(Error::NotFound { .. }))
    ));
    cleanup(&remote, &dir).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn many_operations_share_a_few_connections() {
    let (remote, _dir) = session().await;
    let reads = (0..12).map(|_| {
        let remote = remote.clone();
        async move {
            read_all(&remote, &path("/home/ferge/tree/hello.txt"), 0)
                .await
                .len()
        }
    });
    assert_eq!(futures::future::join_all(reads).await, [34; 12]);
    // A listing dropped half-way leaves the pool usable.
    let mut stream = remote.list(&path("/home/ferge/many"));
    stream.next().await.unwrap().unwrap();
    drop(stream);
    assert_eq!(
        read_all(&remote, &path("/home/ferge/tree/hello.txt"), 0)
            .await
            .len(),
        34
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the FTP test server (./testserver/up.sh)"]
async fn transfers_trees_up_and_down() {
    let (remote, _dir) = session().await;
    let ftp: Arc<dyn Remote> = Arc::new(remote.clone());
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let from = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(from.path().join("project/sub")).unwrap();
    std::fs::write(from.path().join("project/a.txt"), b"alpha").unwrap();
    let big: Vec<u8> = (0..5_000_000_u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(from.path().join("project/sub/big.bin"), &big).unwrap();
    let old = UNIX_EPOCH + Duration::from_mins(25_000_000);
    std::fs::File::options()
        .write(true)
        .open(from.path().join("project/a.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let up = scratch(&remote, "up").await;

    let (queue, _events) = TransferQueue::new(4);
    let settle = |queue: TransferQueue, id| async move {
        loop {
            let snapshot = queue.snapshot().into_iter().find(|s| s.id == id).unwrap();
            if snapshot.state.is_settled() {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&local),
            items: vec![path(from.path().join("project").to_str().unwrap())],
            target: Arc::clone(&ftp),
            target_dir: up.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let done = settle(queue.clone(), id).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.errors);
    let project = up.join(&ferge_core::EntryName::new("project").unwrap());
    assert_eq!(
        names(&remote, &project).await,
        [
            ("a.txt".to_owned(), EntryKind::File),
            ("sub".to_owned(), EntryKind::Directory)
        ],
        "no partial files left"
    );
    let a = project.join(&ferge_core::EntryName::new("a.txt").unwrap());
    assert_eq!(remote.stat(&a).await.unwrap().modified, Some(old));

    let back = tempfile::tempdir().unwrap();
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&ftp),
            items: vec![project.clone()],
            target: Arc::clone(&local),
            target_dir: path(back.path().to_str().unwrap()),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let done = settle(queue.clone(), id).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.errors);
    assert_eq!(
        std::fs::read(back.path().join("project/sub/big.bin")).unwrap(),
        big
    );
    assert_eq!(
        std::fs::read(back.path().join("project/a.txt")).unwrap(),
        b"alpha"
    );
    cleanup(&remote, &up).await;
}
