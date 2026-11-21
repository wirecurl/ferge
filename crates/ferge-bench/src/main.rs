//! Checks Ferge against the performance budgets in PLAN.md.
//!
//! Needs the release build of the app (`cargo build --release`) and the test
//! server (`./testserver/up.sh`). Exits with status 1 if a budget is missed.

#![allow(
    clippy::print_stdout,
    reason = "the report on stdout is the purpose of this tool"
)]

mod app;
mod core;
mod report;
mod stats;
mod transfer;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, ensure};
use clap::Parser;
use ferge_core::RemotePath;

use crate::app::AppUnderTest;
use crate::core::FRAME_BUDGET;
use crate::report::Verdict;
use crate::stats::{Summary, ms};

/// Cold start budget from PLAN.md.
const STARTUP_BUDGET: Duration = Duration::from_millis(150);

/// Number of entries in the large test directory.
const LARGE_DIRECTORY: usize = 100_000;

#[derive(Debug, Parser)]
#[command(
    name = "ferge-bench",
    about = "Measures Ferge against its performance budgets"
)]
struct Cli {
    /// The app binary. Defaults to the workspace's release build.
    #[arg(long)]
    app: Option<PathBuf>,
    /// SSH configuration that defines the host alias `ferge-test`.
    #[arg(long)]
    ssh_config: Option<PathBuf>,
    /// Directory with 100,000 entries on the test server.
    #[arg(long, default_value = "/home/ferge/many")]
    large_dir: String,
    /// Cold starts to measure (one extra warm-up run is discarded).
    #[arg(long, default_value_t = 15)]
    startup_runs: usize,
    /// Repetitions of the in-process measurements.
    #[arg(long, default_value_t = 5)]
    runs: usize,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("ferge-bench: {error:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<bool> {
    let root = workspace_root();
    let binary = cli.app.unwrap_or_else(|| app::default_binary(&root));
    ensure!(
        binary.exists(),
        "{} not found; run `cargo build --release` first",
        binary.display()
    );
    let ssh_config = cli
        .ssh_config
        .unwrap_or_else(|| root.join(".testserver/ssh_config"));
    ensure!(
        ssh_config.exists(),
        "{} not found; run ./testserver/up.sh first",
        ssh_config.display()
    );
    let large_dir = RemotePath::parse(&cli.large_dir)?;
    let app = AppUnderTest {
        binary,
        ssh_config: ssh_config.clone(),
        target: format!("sftp://ferge-test{large_dir}"),
    };

    let startup = measure_startup(&app, cli.startup_runs).await?;
    let core = measure_core(&ssh_config, &large_dir, cli.runs).await?;
    measure_transfers(&ssh_config).await?;
    let ui = measure_ui(&app).await?;
    let verdicts = verdicts(&startup, &core, &ui);
    println!();
    print!("{}", report::table(&verdicts));
    Ok(verdicts.iter().all(|verdict| verdict.met))
}

async fn measure_startup(app: &AppUnderTest, runs: usize) -> anyhow::Result<Summary> {
    println!("Cold start ({runs} runs)…");
    app.startup().await.context("warm-up start")?;
    let mut wall = Vec::with_capacity(runs);
    let mut in_app = Vec::with_capacity(runs);
    for _ in 0..runs {
        let startup = app.startup().await?;
        wall.push(startup.wall);
        in_app.push(startup.in_app);
    }
    let (wall, in_app) = (Summary::new(wall), Summary::new(in_app));
    println!("  process spawn → first frame presented: {wall}");
    println!("  main() → first frame presented:        {in_app}");
    Ok(wall)
}

struct CoreResults {
    listing: core::Listing,
    cache_hit: Summary,
}

async fn measure_core(
    ssh_config: &Path,
    large_dir: &RemotePath,
    runs: usize,
) -> anyhow::Result<CoreResults> {
    println!("Core, {runs} runs against the test server…");
    let remote = core::connect(ssh_config, "ferge-test").await?;
    let listing = core::listing(&remote, large_dir, runs).await?;
    ensure!(
        listing.entries == LARGE_DIRECTORY,
        "expected {LARGE_DIRECTORY} entries in {large_dir}, found {}",
        listing.entries
    );
    println!("  first chunk:     {}", listing.first_chunk);
    println!("  complete:        {}", listing.complete);
    let cache_hit = core::cache_hit(&remote, large_dir, runs).await?;
    println!("  cache hit:       {cache_hit}");
    let costs = core::view_costs(&listing.sample, runs);
    println!("  build + sort:    {}", costs.build);
    println!("  re-sort (job):   {}", costs.sort_job);
    println!("  direction flip:  {}", costs.flip);
    println!("  filter:          {}", costs.filter);
    Ok(CoreResults { listing, cache_hit })
}

/// Reports transfer throughput; there is no budget for it.
async fn measure_transfers(ssh_config: &Path) -> anyhow::Result<()> {
    println!(
        "Transfers, one {} MiB file…",
        transfer::SIZE / (1024 * 1024)
    );
    let remote = core::connect(ssh_config, "ferge-test").await?;
    let scratch = tempfile::tempdir()?;
    let result = transfer::measure(&remote, ssh_config, scratch.path()).await?;
    #[expect(
        clippy::cast_precision_loss,
        reason = "megabytes per second need no exact bytes"
    )]
    let rate = |elapsed: Duration| transfer::SIZE as f64 / 1e6 / elapsed.as_secs_f64();
    println!(
        "  upload:          {} ({:.0} MB/s)",
        ms(result.upload),
        rate(result.upload)
    );
    println!(
        "  download:        {} ({:.0} MB/s)",
        ms(result.download),
        rate(result.download)
    );
    if let Some(openssh) = result.openssh_download {
        println!(
            "  OpenSSH sftp:    {} ({:.0} MB/s) for comparison",
            ms(openssh),
            rate(openssh)
        );
    }
    Ok(())
}

struct UiResults {
    first_rows: Duration,
    listing_complete: Duration,
    key_frames: Summary,
    scroll_frames: Summary,
    sort_frames: Summary,
    navigate: Duration,
}

async fn measure_ui(app: &AppUnderTest) -> anyhow::Result<UiResults> {
    println!("UI scenario (keys, scrolling, filter, sorting, cached navigation)…");
    let ui = app.interaction().await?;
    ensure!(
        ui.entries == LARGE_DIRECTORY,
        "the app listed {} entries, expected {LARGE_DIRECTORY}",
        ui.entries
    );
    let results = UiResults {
        first_rows: ui.first_rows,
        listing_complete: ui.listing_complete,
        key_frames: Summary::new(ui.input_frames_of(&["keys-", "filter"])),
        scroll_frames: Summary::new(ui.frames_of(&["scroll", "keys-pagedown"])),
        sort_frames: Summary::new(ui.frames_of(&["sort-"])),
        navigate: ui
            .settled
            .get("navigate-cached")
            .copied()
            .context("missing cached navigation result")?,
    };
    let sort_settled = ["sort-size", "sort-modified", "sort-name"]
        .iter()
        .filter_map(|phase| ui.settled.get(*phase).copied())
        .max()
        .unwrap_or_default();
    println!(
        "  first rows:      {} (complete {})",
        ms(ui.first_rows),
        ms(ui.listing_complete)
    );
    println!("  input → frame:   {}", results.key_frames);
    println!("  scroll frames:   {}", results.scroll_frames);
    println!(
        "  sort frames:     {} (rows reordered after ≤ {})",
        results.sort_frames,
        ms(sort_settled)
    );
    println!("  cached folder:   {} until shown", ms(results.navigate));
    Ok(results)
}

fn frames_verdict(budget: &'static str, frames: &Summary) -> Verdict {
    Verdict {
        budget,
        target: format!("no frame over {}", ms(FRAME_BUDGET)),
        measured: format!(
            "{} of {} frames over · max {}",
            frames.count_above(FRAME_BUDGET),
            frames.len(),
            ms(frames.max())
        ),
        met: frames.len() > 0 && frames.count_above(FRAME_BUDGET) == 0,
    }
}

fn verdicts(startup: &Summary, core: &CoreResults, ui: &UiResults) -> Vec<Verdict> {
    let listing = &core.listing;
    vec![
        Verdict {
            budget: "Cold start to usable window",
            target: format!("< {} (p95)", ms(STARTUP_BUDGET)),
            measured: format!(
                "p50 {} · p95 {}",
                ms(startup.median()),
                ms(startup.percentile(95))
            ),
            met: startup.percentile(95) < STARTUP_BUDGET,
        },
        Verdict {
            budget: "Key press to frame",
            target: format!("next frame, ≤ {}", ms(FRAME_BUDGET)),
            measured: format!(
                "p95 {} · max {} ({} keys)",
                ms(ui.key_frames.percentile(95)),
                ms(ui.key_frames.max()),
                ui.key_frames.len()
            ),
            met: ui.key_frames.len() > 0 && ui.key_frames.max() <= FRAME_BUDGET,
        },
        frames_verdict("Scroll 100,000 entries", &ui.scroll_frames),
        frames_verdict("Sort 100,000 entries", &ui.sort_frames),
        Verdict {
            budget: "Known folder from cache",
            target: format!("shown within one frame ({})", ms(FRAME_BUDGET)),
            measured: format!(
                "{} (core cache hit {})",
                ms(ui.navigate),
                ms(core.cache_hit.median())
            ),
            met: ui.navigate <= FRAME_BUDGET,
        },
        Verdict {
            budget: "First rows before listing completes",
            target: "first rows < complete listing".into(),
            measured: format!(
                "UI {} vs {} · core {} vs {}",
                ms(ui.first_rows),
                ms(ui.listing_complete),
                ms(listing.first_chunk.median()),
                ms(listing.complete.median())
            ),
            met: ui.first_rows < ui.listing_complete
                && listing.first_chunk.percentile(95) < listing.complete.median(),
        },
    ]
}
