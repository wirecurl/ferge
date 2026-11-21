//! Throughput of the transfer engine against the test server, with OpenSSH's
//! `sftp` as a point of reference. Reported, not judged: PLAN.md sets no
//! throughput budget, and loopback numbers say little about real links.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use ferry_core::remote::local::LocalRemote;
use ferry_core::transfer::{
    ConflictAction, FixedPolicy, TransferId, TransferQueue, TransferRequest, TransferState,
};
use ferry_core::{EntryName, Remote, RemotePath};

/// Size of the test file.
pub(crate) const SIZE: u64 = 256 * 1024 * 1024;

/// Measured transfer durations.
#[derive(Debug)]
pub(crate) struct Throughput {
    pub upload: Duration,
    pub download: Duration,
    /// The same download with OpenSSH's `sftp`, if it is installed.
    pub openssh_download: Option<Duration>,
}

/// Uploads and downloads a [`SIZE`] file through a [`TransferQueue`].
pub(crate) async fn measure(
    remote: &Arc<dyn Remote>,
    ssh_config: &Path,
    scratch: &Path,
) -> anyhow::Result<Throughput> {
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let source = scratch.join("throughput.bin");
    write_test_file(&source)?;
    let local_dir = RemotePath::parse(scratch.to_str().context("non-UTF-8 temp dir")?)?;
    let remote_dir = remote.home_dir().await?.join(&EntryName::new(&format!(
        "ferry-bench-{}",
        std::process::id()
    ))?);
    remote.mkdir(&remote_dir).await?;

    let (queue, _events) = TransferQueue::new(4);
    let started = Instant::now();
    run(
        &queue,
        TransferRequest {
            source: Arc::clone(&local),
            items: vec![local_dir.join(&EntryName::new("throughput.bin")?)],
            target: Arc::clone(remote),
            target_dir: remote_dir.clone(),
        },
    )
    .await?;
    let upload = started.elapsed();

    let back = scratch.join("back");
    std::fs::create_dir(&back)?;
    let back_dir = RemotePath::parse(back.to_str().context("non-UTF-8 temp dir")?)?;
    let remote_file = remote_dir.join(&EntryName::new("throughput.bin")?);
    let started = Instant::now();
    run(
        &queue,
        TransferRequest {
            source: Arc::clone(remote),
            items: vec![remote_file.clone()],
            target: local,
            target_dir: back_dir,
        },
    )
    .await?;
    let download = started.elapsed();
    ensure!(
        std::fs::metadata(back.join("throughput.bin"))?.len() == SIZE,
        "the downloaded file has the wrong size"
    );

    let openssh_download = openssh_get(ssh_config, remote_file.as_str(), scratch).await;
    remote.delete_file(&remote_file).await?;
    remote.delete_dir(&remote_dir).await?;
    Ok(Throughput {
        upload,
        download,
        openssh_download,
    })
}

async fn run(queue: &TransferQueue, request: TransferRequest) -> anyhow::Result<()> {
    let id = queue.enqueue(request, Arc::new(FixedPolicy(ConflictAction::Overwrite)));
    let state = settled(queue, id).await;
    if state != TransferState::Completed {
        let errors = queue
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .map(|snapshot| snapshot.errors)
            .unwrap_or_default();
        bail!("the transfer ended as {state:?}: {errors:?}");
    }
    Ok(())
}

async fn settled(queue: &TransferQueue, id: TransferId) -> TransferState {
    loop {
        let state = queue
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .map_or(TransferState::Failed, |snapshot| snapshot.state);
        if state.is_settled() {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Incompressible-looking data, so no layer can cheat.
fn write_test_file(path: &Path) -> std::io::Result<()> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut data = Vec::with_capacity(usize::try_from(SIZE).unwrap_or(usize::MAX));
    while (data.len() as u64) < SIZE {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    std::fs::write(path, data)
}

/// Downloads `remote_path` with `sftp -b`; `None` if it is not installed or fails.
async fn openssh_get(ssh_config: &Path, remote_path: &str, scratch: &Path) -> Option<Duration> {
    let target = scratch.join("openssh.bin");
    let batch = format!("get \"{remote_path}\" \"{}\"\n", target.display());
    let batch_file = scratch.join("batch");
    std::fs::write(&batch_file, batch).ok()?;
    let started = Instant::now();
    let status = tokio::process::Command::new("sftp")
        .arg("-q")
        .arg("-F")
        .arg(ssh_config)
        .arg("-b")
        .arg(&batch_file)
        .arg("ferry-test")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .ok()?;
    let elapsed = started.elapsed();
    (status.success() && std::fs::metadata(&target).ok()?.len() == SIZE).then_some(elapsed)
}
