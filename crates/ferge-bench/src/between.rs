//! Copies between the two test servers on every route: through this
//! computer, directly (push and pull), and within one server. Reported, not
//! judged: loopback says little about real links, but the comparison shows
//! what each route costs per byte and per file.
//!
//! The servers reach each other as `sshd:22` and `sshd2:22` inside the
//! compose network; `FERGE_DIRECT_ADDRESS` replaces that. `FERGE_TRACE_DIR`
//! keeps a trace of every run for `ferge-trace`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use bytes::Bytes;
use ferge_core::transfer::route::{QueueOptions, Route, RoutePolicy};
use ferge_core::transfer::{
    ConflictAction, FixedPolicy, TransferQueue, TransferRequest, TransferSnapshot,
};
use ferge_core::{EntryName, Remote, RemotePath};
use futures::StreamExt as _;

/// Size of the large file.
const BIG: u64 = 256 * 1024 * 1024;

/// Number of small files.
const SMALL: usize = 2_000;

/// One measured run.
#[derive(Debug)]
pub(crate) struct Run {
    pub label: &'static str,
    pub route: Option<Route>,
    pub big: Duration,
    pub small: Duration,
}

/// A way to copy, as configured for the queue.
struct Way {
    label: &'static str,
    policy: RoutePolicy,
    prefer_pull: bool,
    same_server: bool,
}

const WAYS: &[Way] = &[
    Way {
        label: "through this computer",
        policy: RoutePolicy::RelayOnly,
        prefer_pull: false,
        same_server: false,
    },
    Way {
        label: "direct, push",
        policy: RoutePolicy::DirectOnly,
        prefer_pull: false,
        same_server: false,
    },
    Way {
        label: "direct, pull",
        policy: RoutePolicy::DirectOnly,
        prefer_pull: true,
        same_server: false,
    },
    Way {
        label: "within one server",
        policy: RoutePolicy::Auto,
        prefer_pull: false,
        same_server: true,
    },
];

/// Measures every way; `None` when the second server is not running.
pub(crate) async fn measure(ssh_config: &Path) -> anyhow::Result<Option<Vec<Run>>> {
    let one = crate::core::connect(ssh_config, "ferge-test").await?;
    let Ok(two) = crate::core::connect(ssh_config, "ferge-test2").await else {
        return Ok(None);
    };
    let one_again = crate::core::connect(ssh_config, "ferge-test").await?;
    let source = scratch(&one, "source").await?;
    let big = source.join(&EntryName::new("big.bin")?);
    put(&one, &big, pseudo_random(BIG)).await?;
    let small = source.join(&EntryName::new("small")?);
    one.mkdir(&small).await?;
    let writes = (0..SMALL).map(|index| {
        let remote = Arc::clone(&one);
        let path = small.join(&EntryName::new(&format!("file-{index:05}")).ok()?);
        Some(async move { put(&remote, &path, format!("small file {index}\n").into_bytes()).await })
    });
    let results: Vec<anyhow::Result<()>> = futures::stream::iter(writes.flatten())
        .buffer_unordered(32)
        .collect()
        .await;
    results.into_iter().collect::<anyhow::Result<Vec<()>>>()?;

    let mut runs = Vec::new();
    for way in WAYS {
        let target = if way.same_server { &one_again } else { &two };
        let started = Instant::now();
        let first = copy(&one, &big, target, way).await?;
        let big_time = started.elapsed();
        let started = Instant::now();
        copy(&one, &small, target, way).await?;
        runs.push(Run {
            label: way.label,
            route: first.route,
            big: big_time,
            small: started.elapsed(),
        });
    }
    one.delete_file(&big).await.ok();
    Ok(Some(runs))
}

/// Copies `item` from `source` into a fresh folder on `target`.
async fn copy(
    source: &Arc<dyn Remote>,
    item: &RemotePath,
    target: &Arc<dyn Remote>,
    way: &Way,
) -> anyhow::Result<TransferSnapshot> {
    let target_dir = scratch(target, "target").await?;
    let mut options = QueueOptions::new(4).with_env();
    options.route = way.policy;
    options.direct.prefer_pull = way.prefer_pull;
    for (remote, inside) in [(source, "sshd:22"), (target, "sshd2:22")] {
        if let Some(server) = remote.server_side() {
            options
                .direct
                .addresses
                .push((server.endpoint().address(), inside.to_owned()));
        }
    }
    let (queue, _events) = TransferQueue::with_options(options);
    let id = queue.enqueue(
        TransferRequest {
            source: Arc::clone(source),
            items: vec![item.clone()],
            target: Arc::clone(target),
            target_dir,
        },
        Arc::new(FixedPolicy(ConflictAction::Overwrite)),
    );
    loop {
        let snapshot = queue
            .snapshot()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .context("transfer vanished")?;
        if snapshot.state.is_settled() {
            if snapshot.state != ferge_core::transfer::TransferState::Completed {
                bail!(
                    "{}: ended as {:?}: {:?}",
                    way.label,
                    snapshot.state,
                    snapshot.errors
                );
            }
            return Ok(snapshot);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn scratch(remote: &Arc<dyn Remote>, label: &str) -> anyhow::Result<RemotePath> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let dir = remote
        .home_dir()
        .await?
        .join(&EntryName::new(&format!("ferge-bench-{label}-{stamp}"))?);
    remote.mkdir(&dir).await?;
    Ok(dir)
}

async fn put(remote: &Arc<dyn Remote>, path: &RemotePath, data: Vec<u8>) -> anyhow::Result<()> {
    let stream = futures::stream::once(async move { Ok(Bytes::from(data)) }).boxed();
    remote.write(path, stream).await?;
    Ok(())
}

/// Incompressible-looking data (xorshift64).
fn pseudo_random(size: u64) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(usize::MAX));
    while (data.len() as u64) < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data
}

/// The report lines.
pub(crate) fn print(runs: &[Run]) {
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let rate = |elapsed: Duration| BIG as f64 / 1e6 / elapsed.as_secs_f64();
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let files = |elapsed: Duration| SMALL as f64 / elapsed.as_secs_f64();
    println!(
        "  {:<24} {:>22} {:>24}",
        "",
        format!("{} MiB file", BIG / (1024 * 1024)),
        format!("{SMALL} small files")
    );
    for run in runs {
        println!(
            "  {:<24} {:>9.2} s {:>7.0} MB/s {:>9.2} s {:>7.0} files/s   ({})",
            run.label,
            run.big.as_secs_f64(),
            rate(run.big),
            run.small.as_secs_f64(),
            files(run.small),
            run.route.map_or("?", Route::as_str),
        );
    }
}
