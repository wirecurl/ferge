//! Direct transfers between the two Docker test servers, and copies within
//! one server.
//!
//! Start the servers with `./testserver/up.sh`, then run
//! `cargo test -p ferge-core --test direct -- --include-ignored`.
//! `FERGE_TEST_PORT` and `FERGE_TEST_PORT2` select other ports;
//! `FERGE_TEST_TRACE_DIR` keeps the transfer traces there.
//!
//! The tests take turns: each checks that the servers are left exactly as
//! it found them, which another test working there at the same time would
//! spoil.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use ferge_core::interaction::{
    Account, HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt,
};
use ferge_core::remote::sftp::{KnownHostsSource, SftpOptions, SftpRemote, SshConfigSource};
use ferge_core::transfer::route::{QueueOptions, Route, RoutePolicy};
use ferge_core::transfer::{
    Conflict, ConflictAction, ConflictDecision, ConflictResolver, DirectDecision, DirectRequest,
    FixedPolicy, TransferId, TransferQueue, TransferRequest, TransferSnapshot, TransferState,
};
use ferge_core::{EntryName, Error, Remote, RemotePath};
use futures::{StreamExt as _, TryStreamExt as _};
use secrecy::SecretString;

/// One test at a time on the servers.
static SERVERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn state_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.testserver")
}

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(default)
}

fn ports() -> (u16, u16) {
    (
        port("FERGE_TEST_PORT", 2222),
        port("FERGE_TEST_PORT2", 2223),
    )
}

/// Rejects every question: keys and host keys are all prepared.
#[derive(Debug)]
struct Silent;

#[async_trait]
impl Interaction for Silent {
    async fn confirm_host_key(&self, _prompt: HostKeyPrompt) -> HostKeyDecision {
        HostKeyDecision::Reject
    }
    async fn key_passphrase(&self, _key_file: &Path, _retry: bool) -> Option<SecretString> {
        None
    }
    async fn password(&self, _account: &Account, _retry: bool) -> Option<SecretString> {
        None
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
    async fn sudo_password(&self, _account: &Account, _retry: bool) -> Option<SecretString> {
        None
    }
}

async fn connect(port: u16) -> SftpRemote {
    let mut options = SftpOptions::new("127.0.0.1");
    options.port = Some(port);
    options.user = Some("ferge".into());
    options.use_agent = false;
    options.use_default_identities = false;
    options.ssh_config = SshConfigSource::Disabled;
    options.known_hosts = KnownHostsSource::File(state_dir().join("known_hosts"));
    options.identity_files = vec![state_dir().join("id_ed25519")];
    SftpRemote::connect(options, Arc::new(Silent))
        .await
        .expect("test server reachable (./testserver/up.sh)")
}

/// Both servers as the other one reaches them inside the compose network.
fn options(route: RoutePolicy, trace_dir: &Path) -> QueueOptions {
    let (one, two) = ports();
    let mut options = QueueOptions::new(4);
    options.route = route;
    // FERGE_TEST_TRACE_DIR keeps the traces for a look with ferge-trace.
    options.trace_dir = Some(
        std::env::var_os("FERGE_TEST_TRACE_DIR")
            .map_or_else(|| trace_dir.to_path_buf(), PathBuf::from),
    );
    options.direct.poll_interval = Duration::from_millis(50);
    options.direct.addresses = vec![
        (format!("127.0.0.1:{one}"), "sshd:22".into()),
        (format!("127.0.0.1:{two}"), "sshd2:22".into()),
    ];
    options
}

fn name(text: &str) -> EntryName {
    EntryName::new(text).unwrap()
}

/// A fresh folder below the login directory.
async fn scratch(remote: &SftpRemote, label: &str) -> RemotePath {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = remote
        .home_dir()
        .await
        .unwrap()
        .join(&name(&format!("direct-{label}-{stamp}")));
    remote.mkdir(&dir).await.unwrap();
    dir
}

async fn put(remote: &SftpRemote, path: &RemotePath, contents: Vec<u8>) {
    let stream = futures::stream::once(async move { Ok(Bytes::from(contents)) }).boxed();
    remote.write(path, stream).await.unwrap();
}

async fn get(remote: &SftpRemote, path: &RemotePath) -> Vec<u8> {
    let chunks: Vec<Bytes> = remote
        .read(path)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    chunks.concat()
}

const NAMES: &[&str] = &[
    "plain.txt",
    "with space 'quote' \"double\"",
    "dollar $HOME `id` $(id)",
    "back\\slash %s",
    "-dash",
    "ümlaut 日本 🦀",
    "glob * ? [a-z] ;&|",
];

fn contents(index: usize) -> Vec<u8> {
    (0..index * 9_973 + 17)
        .map(|byte| u8::try_from((byte * 31 + index) % 256).unwrap())
        .collect()
}

/// A folder with odd names, a subfolder and a larger file; returns it with
/// the relative paths and contents it holds.
async fn fixture(remote: &SftpRemote, label: &str) -> (RemotePath, Vec<(String, Vec<u8>)>) {
    let dir = scratch(remote, label).await;
    let root = dir.join(&name("tree"));
    remote.mkdir(&root).await.unwrap();
    let sub = root.join(&name("sub dir"));
    remote.mkdir(&sub).await.unwrap();
    let mut files = Vec::new();
    for (index, file) in NAMES.iter().enumerate() {
        let data = contents(index);
        put(remote, &root.join(&name(file)), data.clone()).await;
        files.push(((*file).to_owned(), data));
    }
    let big: Vec<u8> = (0..5 * 1024 * 1024u32)
        .map(|byte| (byte % 251) as u8)
        .collect();
    put(remote, &sub.join(&name("big.bin")), big.clone()).await;
    files.push(("sub dir/big.bin".to_owned(), big));
    let modified = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    remote
        .set_modified(&root.join(&name("plain.txt")), modified)
        .await
        .unwrap();
    (root, files)
}

async fn wait_for(
    queue: &TransferQueue,
    id: TransferId,
    condition: impl Fn(&TransferSnapshot) -> bool,
) -> TransferSnapshot {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let snapshot = queue
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .unwrap();
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "timed out: {snapshot:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn settled(queue: &TransferQueue, id: TransferId) -> TransferSnapshot {
    wait_for(queue, id, |snapshot| snapshot.state.is_settled()).await
}

fn path_below(root: &RemotePath, relative: &str) -> RemotePath {
    relative
        .split('/')
        .fold(root.clone(), |path, part| path.join(&name(part)))
}

async fn assert_copied(target: &SftpRemote, root: &RemotePath, files: &[(String, Vec<u8>)]) {
    for (relative, data) in files {
        let path = path_below(root, relative);
        assert_eq!(&get(target, &path).await, data, "{relative}");
    }
    let plain = target.stat(&root.join(&name("plain.txt"))).await.unwrap();
    assert_eq!(
        plain.modified,
        Some(UNIX_EPOCH + Duration::from_secs(1_600_000_000))
    );
}

/// Nothing of the transfer's jobs is left: no key, no job folder; and ferge
/// found every server it touched as it was before.
async fn assert_clean(remote: &SftpRemote, snapshot: &TransferSnapshot) {
    let events = trace(snapshot);
    for event in events
        .iter()
        .filter(|line| line["ev"] == "direct.footprint")
    {
        assert_eq!(event["as_found"], true, "{event}");
    }
    let keys = remote
        .home_dir()
        .await
        .unwrap()
        .join(&name(".ssh"))
        .join(&name("authorized_keys"));
    let text = String::from_utf8(get(remote, &keys).await).unwrap();
    for job in jobs(snapshot) {
        assert!(!text.contains(&format!("ferge-job-{job}")), "{text}");
        let folder = job_folder(remote, &job).await;
        assert!(
            matches!(remote.stat(&folder).await, Err(Error::NotFound { .. })),
            "{folder} is left"
        );
    }
}

async fn job_folder(remote: &SftpRemote, job: &str) -> RemotePath {
    RemotePath::parse(&format!(
        "{}/.cache/ferge/jobs/{job}",
        remote.home_dir().await.unwrap()
    ))
    .unwrap()
}

/// The ids of the jobs a transfer set up.
fn jobs(snapshot: &TransferSnapshot) -> Vec<String> {
    trace(snapshot)
        .iter()
        .filter(|line| line["ev"] == "direct.attempt")
        .map(|line| line["job"].as_str().unwrap().to_owned())
        .collect()
}

/// The events of the transfer's trace.
fn trace(snapshot: &TransferSnapshot) -> Vec<serde_json::Value> {
    let path = snapshot.trace.as_ref().expect("trace recorded");
    // The trace is written asynchronously; give it a moment.
    std::thread::sleep(Duration::from_millis(200));
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn direct_copy(prefer_pull: bool, expected: Route) {
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let (root, files) = fixture(&source, "src").await;
    let target_dir = scratch(&target, "dst").await;
    let traces = tempfile::tempdir().unwrap();
    let mut options = options(RoutePolicy::DirectOnly, traces.path());
    options.direct.prefer_pull = prefer_pull;
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root.clone()],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(snapshot.route, Some(expected));
    assert_eq!(snapshot.files_done, files.len() as u64);
    assert_eq!(snapshot.bytes_done, snapshot.bytes_total);
    assert_copied(&target, &target_dir.join(&name("tree")), &files).await;
    assert_clean(&source, &snapshot).await;
    assert_clean(&target, &snapshot).await;

    let events = trace(&snapshot);
    let has = |event: &str| events.iter().any(|line| line["ev"] == event);
    assert!(has("direct.probe") && has("direct.end") && has("artifact"));
    let methods = |method: &str| {
        events
            .iter()
            .filter(|line| line["ev"] == "file.done" && line["method"] == method)
            .count()
    };
    // The small files go in batches, the large one alone.
    assert_eq!(methods("direct-batch"), files.len() - 1);
    assert_eq!(methods("direct"), 1);
    let footprints = events
        .iter()
        .filter(|line| line["ev"] == "direct.footprint")
        .count();
    assert_eq!(footprints, 2);
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn pushes_between_two_servers() {
    let _servers = SERVERS.lock().await;
    direct_copy(false, Route::DirectPush).await;
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn pulls_between_two_servers() {
    let _servers = SERVERS.lock().await;
    direct_copy(true, Route::DirectPull).await;
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn relays_when_the_servers_cannot_reach_each_other() {
    let _servers = SERVERS.lock().await;
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let (root, files) = fixture(&source, "unreachable").await;
    let target_dir = scratch(&target, "unreachable-dst").await;
    let traces = tempfile::tempdir().unwrap();
    let mut options = options(RoutePolicy::Auto, traces.path());
    // Nothing listens there inside the containers.
    options.direct.addresses = vec![
        (format!("127.0.0.1:{one}"), "127.0.0.1:1".into()),
        (format!("127.0.0.1:{two}"), "127.0.0.1:1".into()),
    ];
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(snapshot.route, Some(Route::Relay));
    let note = snapshot.route_note.clone().unwrap_or_default();
    assert!(note.contains("push:") && note.contains("pull:"), "{note}");
    assert!(note.contains("could not log in"), "{note}");
    assert_copied(&target, &target_dir.join(&name("tree")), &files).await;
    assert_clean(&source, &snapshot).await;
    assert_clean(&target, &snapshot).await;
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn copies_within_one_server() {
    let _servers = SERVERS.lock().await;
    let (one, _) = ports();
    // Two sessions, as two panes would have.
    let (source, target) = (connect(one).await, connect(one).await);
    let (root, files) = fixture(&source, "within").await;
    let target_dir = scratch(&target, "within-dst").await;
    let traces = tempfile::tempdir().unwrap();
    let (queue, _events) = TransferQueue::with_options(options(RoutePolicy::Auto, traces.path()));
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(snapshot.route, Some(Route::Server));
    assert_copied(&target, &target_dir.join(&name("tree")), &files).await;
    let methods: Vec<String> = trace(&snapshot)
        .iter()
        .filter(|line| line["ev"] == "file.done")
        .map(|line| line["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(methods.len(), files.len());
    assert!(
        methods.iter().all(|method| method == "copy-data"),
        "{methods:?}"
    );
}

/// Many small files, so a job runs long enough to be paused or abandoned.
async fn many_files(remote: &SftpRemote, label: &str, count: usize) -> RemotePath {
    let dir = scratch(remote, label).await;
    let root = dir.join(&name("many"));
    remote.mkdir(&root).await.unwrap();
    let writes = (0..count).map(|index| {
        let remote = remote.clone();
        let path = root.join(&name(&format!("file-{index:05}")));
        async move { put(&remote, &path, format!("file {index}\n").into_bytes()).await }
    });
    futures::stream::iter(writes)
        .buffer_unordered(32)
        .collect::<Vec<()>>()
        .await;
    root
}

async fn count_files(remote: &SftpRemote, dir: &RemotePath) -> usize {
    match remote.list(dir).try_concat().await {
        Ok(entries) => entries
            .iter()
            .filter(|entry| !entry.name.as_str().ends_with(".ferge-part"))
            .count(),
        Err(_) => 0,
    }
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn pauses_and_resumes_a_direct_job() {
    const COUNT: usize = 600;
    let _servers = SERVERS.lock().await;
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let root = many_files(&source, "pause", COUNT).await;
    let target_dir = scratch(&target, "pause-dst").await;
    let traces = tempfile::tempdir().unwrap();
    let mut options = options(RoutePolicy::DirectOnly, traces.path());
    options.direct.workers = 2;
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    wait_for(&queue, id, |snapshot| snapshot.files_done >= 20).await;
    queue.pause(id);
    let paused = settled(&queue, id).await;
    assert_eq!(paused.state, TransferState::Paused, "{paused:?}");
    assert!(
        paused.files_done < COUNT as u64,
        "finished before the pause"
    );
    // The job really stopped: nothing more arrives.
    let copied = target_dir.join(&name("many"));
    let before = count_files(&target, &copied).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(count_files(&target, &copied).await, before);
    assert_clean(&target, &paused).await;

    queue.resume(id);
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(snapshot.files_done, COUNT as u64);
    assert_eq!(count_files(&target, &copied).await, COUNT);
    assert_clean(&source, &snapshot).await;
    assert_clean(&target, &snapshot).await;
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn a_direct_job_finishes_without_ferge() {
    const COUNT: usize = 400;
    let _servers = SERVERS.lock().await;
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let root = many_files(&source, "orphan", COUNT).await;
    let target_dir = scratch(&target, "orphan-dst").await;
    let traces = tempfile::tempdir().unwrap();
    let mut options = options(RoutePolicy::DirectOnly, traces.path());
    options.direct.workers = 2;
    options.direct.batching = false;
    options.direct.linger = Duration::from_secs(2);
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let running = wait_for(&queue, id, |snapshot| snapshot.files_done >= 10).await;
    // ferge goes away: the connections close mid-job.
    source.disconnect().await;
    target.disconnect().await;
    drop(queue);

    let target = connect(two).await;
    let copied = target_dir.join(&name("many"));
    let deadline = Instant::now() + Duration::from_secs(120);
    while count_files(&target, &copied).await < COUNT {
        assert!(
            Instant::now() < deadline,
            "the job did not finish on its own"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The job removes its key from the passive side when done.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let keys = target
            .home_dir()
            .await
            .unwrap()
            .join(&name(".ssh"))
            .join(&name("authorized_keys"));
        let text = String::from_utf8(get(&target, &keys).await).unwrap();
        if jobs(&running)
            .iter()
            .all(|job| !text.contains(&format!("ferge-job-{job}")))
        {
            break;
        }
        assert!(Instant::now() < deadline, "the job key stayed: {text}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The passive side is gone entirely.
    let [job] = jobs(&running).try_into().unwrap();
    let passive_folder = job_folder(&target, &job).await;
    assert!(matches!(
        target.stat(&passive_folder).await,
        Err(Error::NotFound { .. })
    ));
    // The pushing side waits for ferge a little, then keeps only a result.
    let source = connect(one).await;
    let folder = job_folder(&source, &job).await;
    let result = folder.join(&name("result"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while source.stat(&result).await.is_err() {
        assert!(Instant::now() < deadline, "the job did not tidy itself");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let left: Vec<String> = source
        .list(&folder)
        .try_concat()
        .await
        .unwrap()
        .iter()
        .map(|entry| entry.name.as_str().to_owned())
        .collect();
    // The status stays for ferge to follow the job later.
    let mut left = left;
    left.sort();
    assert_eq!(left, ["before", "result", "status"]);
    let text = String::from_utf8(get(&source, &result).await).unwrap();
    assert!(
        text.contains("ended without ferge") && text.contains("\nE 0 "),
        "{text}"
    );

    next_job_reports(&source, &target, &job, traces.path()).await;
}

/// The next job on the server of a job that ended without ferge reports its
/// result and removes it.
async fn next_job_reports(source: &SftpRemote, target: &SftpRemote, job: &str, traces: &Path) {
    let folder = job_folder(source, job).await;
    let (next_root, _) = fixture(source, "after-orphan").await;
    let next_dir = scratch(target, "after-orphan-dst").await;
    let (queue, _events) = TransferQueue::with_options(options(RoutePolicy::DirectOnly, traces));
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![next_root],
            target: Arc::new(target.clone()),
            target_dir: next_dir,
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    let sweep = trace(&snapshot)
        .into_iter()
        .find(|line| line["ev"] == "direct.sweep" && line["side"] == "active")
        .expect("the result was reported");
    assert!(
        sweep["results"][job].as_str().unwrap().contains("E 0 "),
        "{sweep}"
    );
    assert!(matches!(
        source.stat(&folder).await,
        Err(Error::NotFound { .. })
    ));
    assert_clean(source, &snapshot).await;
    assert_clean(target, &snapshot).await;
}

/// Answers every question about copying directly with "no", and notes
/// whether `look` existed when asked.
#[derive(Debug, Default)]
struct Declines {
    asked: std::sync::Mutex<Vec<DirectRequest>>,
    look: Option<(SftpRemote, RemotePath)>,
    existed: std::sync::Mutex<Vec<bool>>,
}

#[async_trait]
impl ConflictResolver for Declines {
    async fn resolve(&self, _conflict: Conflict) -> Option<ConflictDecision> {
        Some(ConflictDecision {
            action: ConflictAction::Overwrite,
            apply_to_all: true,
        })
    }

    async fn approve_direct(&self, request: DirectRequest) -> DirectDecision {
        self.asked.lock().unwrap().push(request);
        if let Some((remote, path)) = &self.look {
            let exists = remote.lstat(path).await.is_ok();
            self.existed.lock().unwrap().push(exists);
        }
        DirectDecision::Relay
    }
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn asks_before_copying_directly() {
    let _servers = SERVERS.lock().await;
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let (root, files) = fixture(&source, "declined").await;
    let target_dir = scratch(&target, "declined-dst").await;
    let traces = tempfile::tempdir().unwrap();
    let (queue, _events) = TransferQueue::with_options(options(RoutePolicy::Auto, traces.path()));
    let resolver = Arc::new(Declines {
        look: Some((target.clone(), target_dir.join(&name("tree")))),
        ..Declines::default()
    });
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::clone(&resolver) as Arc<dyn ConflictResolver>,
    );
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(snapshot.route, Some(Route::Relay));
    assert!(
        snapshot
            .route_note
            .as_deref()
            .unwrap_or_default()
            .contains("you chose"),
        "{snapshot:?}"
    );
    let asked = resolver.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].source_id, format!("ferge@127.0.0.1:{one}"));
    assert_eq!(asked[0].target_id, format!("ferge@127.0.0.1:{two}"));
    // Nothing was made on the target before the answer.
    assert_eq!(*resolver.existed.lock().unwrap(), [false]);
    // Nothing was set up on the servers.
    assert!(
        !trace(&snapshot)
            .iter()
            .any(|line| line["ev"] == "direct.attempt")
    );
    assert_copied(&target, &target_dir.join(&name("tree")), &files).await;
}

/// Starts a direct job on many files and quits ferge once a few arrived;
/// returns the job's record and the transfer's target folder.
async fn start_and_quit(
    label: &str,
    count: usize,
    records: &Path,
    traces: &Path,
) -> (ferge_core::transfer::direct::JobRecord, RemotePath) {
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let root = many_files(&source, label, count).await;
    let target_dir = scratch(&target, &format!("{label}-dst")).await;
    let mut options = options(RoutePolicy::DirectOnly, traces);
    options.direct.workers = 2;
    options.direct.batching = false;
    options.direct.linger = Duration::from_secs(2);
    options.direct.records = Some(records.to_path_buf());
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    wait_for(&queue, id, |snapshot| snapshot.files_done >= 10).await;
    source.disconnect().await;
    target.disconnect().await;
    drop(queue);
    let mut records = ferge_core::transfer::direct::recorded_jobs(records).await;
    assert_eq!(records.len(), 1, "the job was recorded");
    (records.remove(0), target_dir)
}

/// ferge starts again and follows the recorded job to its end.
async fn follow_again(
    record: ferge_core::transfer::direct::JobRecord,
    count: usize,
    records: &Path,
    traces: &Path,
) {
    let (one, two) = ports();
    assert_eq!(record.source.port, one);
    assert_eq!(record.target.port, two);
    let (source, target) = (connect(one).await, connect(two).await);
    let mut options = options(RoutePolicy::DirectOnly, traces);
    options.direct.records = Some(records.to_path_buf());
    let (queue, _events) = TransferQueue::with_options(options);
    let target_dir = record.target_dir.clone();
    let id = queue
        .attach(
            record.clone(),
            Arc::new(source.clone()),
            Arc::new(target.clone()),
        )
        .unwrap();
    assert!(
        queue
            .attach(record, Arc::new(source.clone()), Arc::new(target.clone()))
            .is_none(),
        "a job is followed once"
    );
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert!(snapshot.attached);
    assert_eq!(snapshot.files_done, count as u64);
    assert_eq!(snapshot.bytes_done, snapshot.bytes_total);
    assert_eq!(
        count_files(&target, &target_dir.join(&name("many"))).await,
        count
    );
    assert!(
        ferge_core::transfer::direct::recorded_jobs(records)
            .await
            .is_empty(),
        "the record is gone once cleaned up"
    );
    assert_clean(&source, &snapshot).await;
    assert_clean(&target, &snapshot).await;
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn follows_a_running_job_after_a_restart() {
    const COUNT: usize = 600;
    let _servers = SERVERS.lock().await;
    let (records, traces) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (record, _) = start_and_quit("attach", COUNT, records.path(), traces.path()).await;
    follow_again(record, COUNT, records.path(), traces.path()).await;
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn follows_a_job_that_ended_meanwhile() {
    const COUNT: usize = 200;
    let _servers = SERVERS.lock().await;
    let (records, traces) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (record, target_dir) =
        start_and_quit("attach-late", COUNT, records.path(), traces.path()).await;
    // Wait until the job ended and tidied itself down to its result.
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let folder = job_folder(&source, &record.job).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while source.stat(&folder.join(&name("result"))).await.is_err() {
        assert!(Instant::now() < deadline, "the job did not tidy itself");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        count_files(&target, &target_dir.join(&name("many"))).await,
        COUNT
    );
    follow_again(record, COUNT, records.path(), traces.path()).await;
    assert!(matches!(
        source.stat(&folder).await,
        Err(Error::NotFound { .. })
    ));
}

/// A large file copied through this computer from one test server to the
/// other, saving the transfer in `records`; returns the queue, its events,
/// the transfer and the file's contents and target.
async fn relay_large(
    label: &str,
    records: &Path,
    traces: &Path,
) -> (
    TransferQueue,
    futures::channel::mpsc::UnboundedReceiver<ferge_core::transfer::TransferEvent>,
    TransferId,
    Vec<u8>,
    RemotePath,
    (SftpRemote, SftpRemote),
) {
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let dir = scratch(&source, label).await;
    let data: Vec<u8> = (0..256 * 1024 * 1024u32)
        .map(|byte| byte.wrapping_mul(2_654_435_761).to_le_bytes()[2])
        .collect();
    let file = dir.join(&name("large.bin"));
    put(&source, &file, data.clone()).await;
    let target_dir = scratch(&target, &format!("{label}-dst")).await;
    let mut options = options(RoutePolicy::RelayOnly, traces);
    options.direct.records = Some(records.to_path_buf());
    let (queue, events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![file],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    (
        queue,
        events,
        id,
        data,
        target_dir.join(&name("large.bin")),
        (source, target),
    )
}

/// Cuts both connections once some bytes arrived; the transfer pauses
/// because the connection was lost.
async fn lose_connections(queue: &TransferQueue, id: TransferId, sides: (SftpRemote, SftpRemote)) {
    wait_for(queue, id, |snapshot| snapshot.bytes_done > 16 * 1024 * 1024).await;
    sides.0.disconnect().await;
    sides.1.disconnect().await;
    let paused = settled(queue, id).await;
    assert_eq!(paused.state, TransferState::Paused, "{paused:?}");
    assert!(paused.lost_connection, "{paused:?}");
    assert!(paused.errors.is_empty(), "{paused:?}");
}

async fn reconnected(queue: &TransferQueue, id: TransferId) -> TransferSnapshot {
    let (one, two) = ports();
    assert!(queue.reconnect(
        id,
        Arc::new(connect(one).await),
        Arc::new(connect(two).await)
    ));
    queue.resume(id);
    let snapshot = settled(queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert!(!snapshot.lost_connection);
    snapshot
}

fn resumed_at(snapshot: &TransferSnapshot) -> u64 {
    trace(snapshot)
        .iter()
        .filter(|line| line["ev"] == "file.resume")
        .filter_map(|line| line["offset"].as_u64())
        .max()
        .unwrap_or(0)
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn goes_on_after_a_lost_connection() {
    let _servers = SERVERS.lock().await;
    let (records, traces) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (queue, mut events, id, data, copied, sides) =
        relay_large("lost", records.path(), traces.path()).await;
    lose_connections(&queue, id, sides).await;
    let mut lost = false;
    while let Ok(event) = events.try_recv() {
        lost |= matches!(event, ferge_core::transfer::TransferEvent::ConnectionLost(lost) if lost == id);
    }
    assert!(lost, "the queue announced the lost connection");

    let snapshot = reconnected(&queue, id).await;
    assert!(
        resumed_at(&snapshot) > 0,
        "the file went on where it stopped"
    );
    let target = connect(ports().1).await;
    assert_eq!(get(&target, &copied).await, data);
    // Done: nothing is kept for a restart.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_dir(records.path()).unwrap().count(), 0);
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn resumes_a_saved_transfer_after_a_restart() {
    let _servers = SERVERS.lock().await;
    let (records, traces) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (queue, _events, id, data, copied, sides) =
        relay_large("restart", records.path(), traces.path()).await;
    lose_connections(&queue, id, sides).await;
    drop(queue);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ferge starts again.
    let mut saved = ferge_core::transfer::saved_transfers(records.path()).await;
    assert_eq!(saved.len(), 1);
    let saved = saved.remove(0);
    assert_eq!(saved.title, "large.bin");
    let (one, two) = ports();
    assert_eq!(
        saved.target.destination().unwrap().address(),
        format!("sftp://ferge@127.0.0.1:{two}")
    );
    let mut options = options(RoutePolicy::RelayOnly, traces.path());
    options.direct.records = Some(records.path().to_path_buf());
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.restore(&saved, Arc::new(FixedPolicy(ConflictAction::Overwrite)));
    let restored = queue
        .snapshot()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .unwrap();
    assert_eq!(restored.state, TransferState::Paused);
    assert!(!restored.connected);
    assert!(restored.bytes_total >= data.len() as u64);
    // Not connected: resuming does nothing yet.
    queue.resume(id);
    assert_eq!(
        settled(&queue, id).await.state,
        TransferState::Paused,
        "waits for connections"
    );
    assert_eq!(
        restored.target_server.unwrap().address(),
        format!("sftp://ferge@127.0.0.1:{two}")
    );
    let _ = one;

    let snapshot = reconnected(&queue, id).await;
    assert!(
        resumed_at(&snapshot) > 0,
        "the file went on where it stopped"
    );
    let target = connect(two).await;
    assert_eq!(get(&target, &copied).await, data);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_dir(records.path()).unwrap().count(), 0);
}

#[tokio::test]
#[ignore = "needs ./testserver/up.sh"]
async fn follows_a_direct_job_again_after_a_lost_connection() {
    const COUNT: usize = 400;
    let _servers = SERVERS.lock().await;
    let (one, two) = ports();
    let (source, target) = (connect(one).await, connect(two).await);
    let root = many_files(&source, "lost-direct", COUNT).await;
    let target_dir = scratch(&target, "lost-direct-dst").await;
    let traces = tempfile::tempdir().unwrap();
    let mut options = options(RoutePolicy::DirectOnly, traces.path());
    options.direct.workers = 2;
    options.direct.batching = false;
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(source.clone()),
            items: vec![root],
            target: Arc::new(target.clone()),
            target_dir: target_dir.clone(),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    wait_for(&queue, id, |snapshot| snapshot.files_done >= 10).await;
    source.disconnect().await;
    target.disconnect().await;
    let paused = settled(&queue, id).await;
    assert_eq!(paused.state, TransferState::Paused, "{paused:?}");
    assert!(paused.lost_connection && paused.attached, "{paused:?}");

    let (source, target) = (connect(one).await, connect(two).await);
    assert!(queue.reconnect(id, Arc::new(source.clone()), Arc::new(target.clone())));
    queue.resume(id);
    let snapshot = settled(&queue, id).await;
    assert_eq!(snapshot.state, TransferState::Completed, "{snapshot:?}");
    assert_eq!(snapshot.files_done, COUNT as u64);
    assert_eq!(
        count_files(&target, &target_dir.join(&name("many"))).await,
        COUNT
    );
    // One job only: following it again started no second one.
    assert_eq!(jobs(&snapshot).len(), 1);
    assert_clean(&source, &snapshot).await;
    assert_clean(&target, &snapshot).await;
}
