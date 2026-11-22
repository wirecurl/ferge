//! Transfers between local folders; SFTP is covered by the integration tests.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;

use super::*;
use crate::entry::Metadata;
use crate::remote::local::LocalRemote;
use crate::remote::{ByteStream, FileWriter, ListStream};

fn path(path: &Path) -> RemotePath {
    RemotePath::parse(path.to_str().unwrap()).unwrap()
}

fn local() -> Arc<dyn Remote> {
    Arc::new(LocalRemote::new())
}

/// Waits until the transfer has settled and returns its last snapshot.
async fn settled(queue: &TransferQueue, id: TransferId) -> TransferSnapshot {
    wait_for(queue, id, |snapshot| snapshot.state.is_settled()).await
}

async fn wait_for(
    queue: &TransferQueue,
    id: TransferId,
    condition: impl Fn(&TransferSnapshot) -> bool,
) -> TransferSnapshot {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = queue
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .expect("transfer exists");
        if condition(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "timed out: {snapshot:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn write(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn files_below(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            let relative = entry
                .path()
                .strip_prefix(dir)
                .unwrap()
                .display()
                .to_string();
            if entry.file_type().unwrap().is_dir() {
                found.push(format!("{relative}/"));
                pending.push(entry.path());
            } else {
                found.push(relative);
            }
        }
    }
    found.sort();
    found
}

/// Answers conflicts with a fixed decision and counts the questions.
#[derive(Debug)]
struct Answer {
    decision: Option<ConflictDecision>,
    asked: AtomicUsize,
}

impl Answer {
    fn new(action: ConflictAction, apply_to_all: bool) -> Arc<Self> {
        Arc::new(Self {
            decision: Some(ConflictDecision {
                action,
                apply_to_all,
            }),
            asked: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ConflictResolver for Answer {
    async fn resolve(&self, _conflict: Conflict) -> Option<ConflictDecision> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.decision
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn copies_a_tree_and_keeps_times() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..3_000_000_u32).map(|i| (i % 249) as u8).collect();
    write(&from.path().join("project/a.txt"), b"alpha");
    write(&from.path().join("project/sub/b.bin"), &big);
    write(&from.path().join("project/sub/deeper/c"), b"");
    std::fs::create_dir_all(from.path().join("project/empty")).unwrap();
    write(&from.path().join("single.txt"), b"one");
    let old = UNIX_EPOCH + Duration::from_hours(416_666);
    std::fs::File::options()
        .write(true)
        .open(from.path().join("single.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();

    let (queue, _events) = TransferQueue::new(DEFAULT_PARALLEL_FILES);
    let remote = local();
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&remote),
            items: vec![
                path(&from.path().join("project")),
                path(&from.path().join("single.txt")),
            ],
            target: remote,
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let done = settled(&queue, id).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.errors);
    assert_eq!(done.title, "2 items");
    assert_eq!(done.direction, Direction::Copy);
    assert_eq!((done.files_done, done.files_total), (4, 4));
    assert_eq!(done.bytes_done, done.bytes_total);
    assert_eq!(done.bytes_total, 5 + 3 + big.len() as u64);
    assert_eq!(
        files_below(to.path()),
        [
            "project/",
            "project/a.txt",
            "project/empty/",
            "project/sub/",
            "project/sub/b.bin",
            "project/sub/deeper/",
            "project/sub/deeper/c",
            "single.txt",
        ]
    );
    assert_eq!(
        std::fs::read(to.path().join("project/sub/b.bin")).unwrap(),
        big
    );
    let modified = std::fs::metadata(to.path().join("single.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(modified, old);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn never_writes_through_a_planted_partial_file() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    write(&from.path().join("report.pdf"), b"from the server");
    // Someone who can write the target folder links the partial file's
    // name to a file of the user's, and to a file that does not exist yet.
    let victim = elsewhere.path().join("victim");
    write(&victim, b"precious");
    std::os::unix::fs::symlink(&victim, to.path().join("report.pdf.ferge-part")).unwrap();
    write(&from.path().join("notes.txt"), b"notes");
    let planted = elsewhere.path().join("planted");
    std::os::unix::fs::symlink(&planted, to.path().join("notes.txt.ferge-part")).unwrap();

    let (queue, _events) = TransferQueue::new(DEFAULT_PARALLEL_FILES);
    let remote = local();
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&remote),
            items: vec![
                path(&from.path().join("report.pdf")),
                path(&from.path().join("notes.txt")),
            ],
            target: remote,
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let done = settled(&queue, id).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.errors);
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert!(!planted.exists());
    assert_eq!(
        std::fs::read(to.path().join("report.pdf")).unwrap(),
        b"from the server"
    );
    assert!(
        !std::fs::symlink_metadata(to.path().join("report.pdf"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(files_below(to.path()), ["notes.txt", "report.pdf"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_conflicts() {
    for (action, expected_a, extra) in [
        (ConflictAction::Skip, &b"old a"[..], None),
        (ConflictAction::Overwrite, &b"new a"[..], None),
        (ConflictAction::KeepBoth, &b"old a"[..], Some("a (2).txt")),
    ] {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        write(&from.path().join("a.txt"), b"new a");
        write(&from.path().join("b.txt"), b"new b");
        write(&to.path().join("a.txt"), b"old a");
        write(&to.path().join("b.txt"), b"old b");

        let (queue, _events) = TransferQueue::new(2);
        let answer = Answer::new(action, true);
        let id = queue.enqueue(
            TransferRequest {
                source: local(),
                items: vec![
                    path(&from.path().join("a.txt")),
                    path(&from.path().join("b.txt")),
                ],
                target: local(),
                target_dir: path(to.path()),
            },
            Arc::clone(&answer) as Arc<dyn ConflictResolver>,
        );
        let done = settled(&queue, id).await;
        assert_eq!(done.state, TransferState::Completed, "{action:?}");
        // "For all" means one question for both files.
        assert_eq!(answer.asked.load(Ordering::SeqCst), 1, "{action:?}");
        assert_eq!(std::fs::read(to.path().join("a.txt")).unwrap(), expected_a);
        if let Some(extra) = extra {
            assert_eq!(std::fs::read(to.path().join(extra)).unwrap(), b"new a");
        }
        if action == ConflictAction::Skip {
            assert_eq!(done.files_skipped, 2);
        }
        assert!(
            !files_below(to.path())
                .iter()
                .any(|name| name.ends_with(PART_SUFFIX))
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn asks_per_file_without_apply_to_all_and_cancels_when_dismissed() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        write(&from.path().join(name), b"new");
        write(&to.path().join(name), b"old");
    }
    let items: Vec<RemotePath> = ["a", "b", "c"]
        .iter()
        .map(|name| path(&from.path().join(name)))
        .collect();

    let (queue, _events) = TransferQueue::new(1);
    let answer = Answer::new(ConflictAction::Overwrite, false);
    let id = queue.enqueue(
        TransferRequest {
            source: local(),
            items: items.clone(),
            target: local(),
            target_dir: path(to.path()),
        },
        Arc::clone(&answer) as Arc<dyn ConflictResolver>,
    );
    assert_eq!(settled(&queue, id).await.state, TransferState::Completed);
    assert_eq!(answer.asked.load(Ordering::SeqCst), 3);

    let dismissed = Arc::new(Answer {
        decision: None,
        asked: AtomicUsize::new(0),
    });
    let id = queue.enqueue(
        TransferRequest {
            source: local(),
            items,
            target: local(),
            target_dir: path(to.path()),
        },
        dismissed,
    );
    assert_eq!(settled(&queue, id).await.state, TransferState::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicates_in_place_and_refuses_recursion() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("folder/inner.txt"), b"x");
    write(&dir.path().join("note.txt"), b"n");
    let remote = local();
    let (queue, _events) = TransferQueue::new(2);

    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&remote),
            items: vec![path(&dir.path().join("note.txt"))],
            target: Arc::clone(&remote),
            target_dir: path(dir.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    assert_eq!(settled(&queue, id).await.state, TransferState::Completed);
    assert_eq!(
        std::fs::read(dir.path().join("note (2).txt")).unwrap(),
        b"n"
    );

    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&remote),
            items: vec![path(&dir.path().join("folder"))],
            target: remote,
            target_dir: path(&dir.path().join("folder")),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let failed = settled(&queue, id).await;
    assert_eq!(failed.state, TransferState::Failed);
    assert!(
        failed.errors[0].contains("into itself"),
        "{:?}",
        failed.errors
    );
}

/// Local files, read slowly, so a test can pause in the middle of a file.
#[derive(Debug, Default)]
struct Slow {
    inner: LocalRemote,
    /// Offsets reads started at, to see that a resume continued.
    reads_from: Mutex<Vec<u64>>,
    /// Reads never end and are not slowed down: a source that keeps
    /// sending past the size it reported.
    endless: bool,
}

#[async_trait]
impl Remote for Slow {
    fn label(&self) -> &'static str {
        "slow"
    }
    fn is_local(&self) -> bool {
        false
    }
    async fn home_dir(&self) -> crate::Result<RemotePath> {
        self.inner.home_dir().await
    }
    async fn canonicalize(&self, path: &RemotePath) -> crate::Result<RemotePath> {
        self.inner.canonicalize(path).await
    }
    fn list(&self, path: &RemotePath) -> ListStream {
        self.inner.list(path)
    }
    async fn stat(&self, path: &RemotePath) -> crate::Result<Metadata> {
        self.inner.stat(path).await
    }
    async fn lstat(&self, path: &RemotePath) -> crate::Result<Metadata> {
        self.inner.lstat(path).await
    }
    async fn read_from(&self, path: &RemotePath, offset: u64) -> crate::Result<ByteStream> {
        self.reads_from.lock().unwrap().push(offset);
        let stream = self.inner.read_from(path, offset).await?;
        if self.endless {
            let chunk = Bytes::from(vec![0_u8; 1024 * 1024]);
            return Ok(stream
                .chain(futures::stream::repeat_with(move || Ok(chunk.clone())))
                .boxed());
        }
        Ok(stream
            .then(|chunk| async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                chunk
            })
            .boxed())
    }
    async fn open_writer(
        &self,
        path: &RemotePath,
        offset: u64,
    ) -> crate::Result<Box<dyn FileWriter>> {
        self.inner.open_writer(path, offset).await
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> crate::Result<()> {
        self.inner.rename(from, to).await
    }
    async fn rename_replace(&self, from: &RemotePath, to: &RemotePath) -> crate::Result<()> {
        self.inner.rename_replace(from, to).await
    }
    async fn set_modified(&self, path: &RemotePath, time: SystemTime) -> crate::Result<()> {
        self.inner.set_modified(path, time).await
    }
    async fn delete_file(&self, path: &RemotePath) -> crate::Result<()> {
        self.inner.delete_file(path).await
    }
    async fn delete_dir(&self, path: &RemotePath) -> crate::Result<()> {
        self.inner.delete_dir(path).await
    }
    async fn mkdir(&self, path: &RemotePath) -> crate::Result<()> {
        self.inner.mkdir(path).await
    }
    async fn chmod(&self, path: &RemotePath, mode: u32) -> crate::Result<()> {
        self.inner.chmod(path, mode).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_sends_without_end_fails_instead_of_filling_the_disk() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    write(&from.path().join("small.txt"), b"tiny");
    let endless = Arc::new(Slow {
        endless: true,
        ..Slow::default()
    });
    let (queue, _events) = TransferQueue::new(DEFAULT_PARALLEL_FILES);
    let id = queue.enqueue(
        TransferRequest {
            source: endless as Arc<dyn Remote>,
            items: vec![path(&from.path().join("small.txt"))],
            target: local(),
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let done = settled(&queue, id).await;
    assert_eq!(done.state, TransferState::Failed);
    assert!(
        done.errors
            .iter()
            .any(|error| error.contains("kept growing")),
        "{:?}",
        done.errors
    );
    assert_eq!(
        files_below(to.path()),
        Vec::<String>::new(),
        "no partial file is left"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tells_which_connections_transfers_use() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    let data = vec![7_u8; 4 * 1024 * 1024];
    write(&from.path().join("big.bin"), &data);
    let slow: Arc<dyn Remote> = Arc::new(Slow::default());
    let other = local();
    let (queue, _events) = TransferQueue::new(2);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&slow),
            items: vec![path(&from.path().join("big.bin"))],
            target: local(),
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    wait_for(&queue, id, |snapshot| {
        snapshot.state == TransferState::Running
    })
    .await;
    assert!(queue.is_using(&slow));
    assert!(
        !queue.is_using(&other),
        "the same kind of remote, but not this one"
    );
    queue.pause(id);
    wait_for(&queue, id, |snapshot| {
        snapshot.state == TransferState::Paused
    })
    .await;
    assert!(
        queue.is_using(&slow),
        "a paused transfer still needs its connection"
    );
    queue.resume(id);
    tokio::time::timeout(Duration::from_secs(30), queue.until_unused(&slow))
        .await
        .unwrap();
    assert_eq!(settled(&queue, id).await.state, TransferState::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn pauses_and_resumes_in_the_middle_of_a_file() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    // 40 chunks of 256 KiB at 20 ms each: plenty of time to pause.
    let data: Vec<u8> = (0..10 * 1024 * 1024_u32)
        .map(|i| (i.wrapping_mul(7) % 251) as u8)
        .collect();
    write(&from.path().join("big.bin"), &data);
    let slow = Arc::new(Slow::default());
    let (queue, _events) = TransferQueue::new(2);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(&slow) as Arc<dyn Remote>,
            items: vec![path(&from.path().join("big.bin"))],
            target: local(),
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    assert_eq!(
        wait_for(&queue, id, |s| s.bytes_done > 2 * 1024 * 1024)
            .await
            .direction,
        Direction::Download
    );
    queue.pause(id);
    let paused = settled(&queue, id).await;
    assert_eq!(paused.state, TransferState::Paused);
    let part = to.path().join(format!("big.bin{PART_SUFFIX}"));
    let partial = std::fs::metadata(&part).unwrap().len();
    assert!(partial > 0 && partial < data.len() as u64);
    assert!(!to.path().join("big.bin").exists());

    queue.resume(id);
    let done = settled(&queue, id).await;
    assert_eq!(done.state, TransferState::Completed, "{:?}", done.errors);
    assert_eq!(done.bytes_done, data.len() as u64);
    assert_eq!(std::fs::read(to.path().join("big.bin")).unwrap(), data);
    assert!(!part.exists());
    let reads = slow.reads_from.lock().unwrap().clone();
    assert_eq!(reads.len(), 2);
    assert!(reads[1] > 0, "the second read must resume: {reads:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_removes_partial_files() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    write(&from.path().join("big.bin"), &vec![7_u8; 8 * 1024 * 1024]);
    let (queue, mut events) = TransferQueue::new(2);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::new(Slow::default()),
            items: vec![path(&from.path().join("big.bin"))],
            target: local(),
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    wait_for(&queue, id, |s| s.bytes_done > 0).await;
    queue.cancel(id);
    assert_eq!(settled(&queue, id).await.state, TransferState::Cancelled);
    // Cleanup finishes before the transfer reports that it settled.
    loop {
        match events.next().await {
            Some(TransferEvent::Settled { id: settled, .. }) if settled == id => break,
            Some(_) => {}
            None => panic!("events ended"),
        }
    }
    assert_eq!(files_below(to.path()), Vec::<String>::new());
    assert!(queue.remove(id));
    assert!(queue.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_items_do_not_stop_the_others() {
    let from = tempfile::tempdir().unwrap();
    let to = tempfile::tempdir().unwrap();
    write(&from.path().join("good.txt"), b"good");
    #[cfg(unix)]
    std::os::unix::fs::symlink(from.path().join("missing"), from.path().join("dangling")).unwrap();
    let (queue, _events) = TransferQueue::new(2);
    let id = queue.enqueue(
        TransferRequest {
            source: local(),
            items: vec![
                path(&from.path().join("dangling")),
                path(&from.path().join("good.txt")),
            ],
            target: local(),
            target_dir: path(to.path()),
        },
        Arc::new(FixedPolicy(ConflictAction::Skip)),
    );
    let done = settled(&queue, id).await;
    assert_eq!(std::fs::read(to.path().join("good.txt")).unwrap(), b"good");
    assert_eq!(done.state, TransferState::Failed);
    assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
    assert!(done.errors[0].contains("dangling"));
}
