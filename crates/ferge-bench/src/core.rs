//! Measurements of ferge-core without a window: streaming listings over SFTP,
//! the listing cache, and sorting/filtering a large directory.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use ferge_core::interaction::NonInteractive;
use ferge_core::listing::{self, DirectoryView, ListingCache, ListingEvent, SortColumn, SortOrder};
use ferge_core::remote::sftp::{SftpOptions, SftpRemote, SshConfigSource};
use ferge_core::{Entry, Remote, RemotePath};
use futures::StreamExt as _;

use crate::stats::Summary;

/// Streaming listing timings over several runs.
#[derive(Debug)]
pub(crate) struct Listing {
    pub(crate) entries: usize,
    pub(crate) first_chunk: Summary,
    pub(crate) complete: Summary,
    /// The last complete listing, reused for the CPU-bound measurements.
    pub(crate) sample: Vec<Entry>,
}

/// Connects to the test server the way the app does, but without prompts.
pub(crate) async fn connect(ssh_config: &Path, host: &str) -> anyhow::Result<Arc<dyn Remote>> {
    let mut options = SftpOptions::new(host);
    options.ssh_config = SshConfigSource::File(ssh_config.to_path_buf());
    options.use_agent = false;
    let remote = SftpRemote::connect(options, Arc::new(NonInteractive))
        .await
        .context("connecting to the test server (is ./testserver/up.sh running?)")?;
    Ok(Arc::new(remote))
}

/// Lists `path` `runs` times and records when the first chunk and the end arrived.
pub(crate) async fn listing(
    remote: &Arc<dyn Remote>,
    path: &RemotePath,
    runs: usize,
) -> anyhow::Result<Listing> {
    let mut first_chunk = Vec::with_capacity(runs);
    let mut complete = Vec::with_capacity(runs);
    let mut sample = Vec::new();
    for _ in 0..runs {
        let started = Instant::now();
        let mut stream = remote.list(path);
        let mut entries = Vec::new();
        let mut first = None;
        while let Some(chunk) = stream.next().await {
            first.get_or_insert_with(|| started.elapsed());
            entries.extend(chunk?);
        }
        complete.push(started.elapsed());
        first_chunk.push(first.context("the listing was empty")?);
        sample = entries;
    }
    Ok(Listing {
        entries: sample.len(),
        first_chunk: Summary::new(first_chunk),
        complete: Summary::new(complete),
        sample,
    })
}

/// Time until `browse` delivers a cached listing.
pub(crate) async fn cache_hit(
    remote: &Arc<dyn Remote>,
    path: &RemotePath,
    runs: usize,
) -> anyhow::Result<Summary> {
    let cache = Arc::new(ListingCache::default());
    // Fill the cache.
    let mut warm = listing::browse(remote, &cache, path);
    while warm.next().await.is_some() {}

    let mut hits = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let mut stream = listing::browse(remote, &cache, path);
        let event = stream.next().await.context("browse ended early")?;
        hits.push(started.elapsed());
        ensure!(
            matches!(event, ListingEvent::Cached(_)),
            "expected a cache hit, got {event:?}"
        );
        // Let the revalidation finish so runs do not overlap.
        while stream.next().await.is_some() {}
    }
    Ok(Summary::new(hits))
}

/// CPU cost of view operations on a large listing.
#[derive(Debug)]
pub(crate) struct ViewCosts {
    pub(crate) build: Summary,
    pub(crate) sort_job: Summary,
    pub(crate) flip: Summary,
    pub(crate) filter: Summary,
}

pub(crate) fn view_costs(entries: &[Entry], runs: usize) -> ViewCosts {
    let by_size = SortOrder {
        column: SortColumn::Size,
        descending: true,
    };
    let (mut build, mut sort_job, mut flip, mut filter) = (vec![], vec![], vec![], vec![]);
    for _ in 0..runs {
        let started = Instant::now();
        let mut view = DirectoryView::with_entries(SortOrder::default(), "", entries.to_vec());
        build.push(started.elapsed());

        if let Some(job) = view.begin_sort(by_size) {
            let started = Instant::now();
            let sorted = job.run();
            sort_job.push(started.elapsed());
            view.finish_sort(sorted);
        }

        let started = Instant::now();
        view.set_sort(by_size.toggled(SortColumn::Size));
        flip.push(started.elapsed());

        let started = Instant::now();
        view.set_filter("r");
        filter.push(started.elapsed());
        view.set_filter("");
    }
    ViewCosts {
        build: Summary::new(build),
        sort_job: Summary::new(sort_job),
        flip: Summary::new(flip),
        filter: Summary::new(filter),
    }
}

/// Duration of one frame at the display rate the budgets assume (120 Hz).
pub(crate) const FRAME_BUDGET: Duration = Duration::from_nanos(8_333_333);
