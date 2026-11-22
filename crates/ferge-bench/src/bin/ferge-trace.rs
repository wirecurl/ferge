//! Summarizes transfer traces (see `ferge_core::transfer::trace`).
//!
//! ```sh
//! cargo run -p ferge-bench --bin ferge-trace            # the newest trace
//! cargo run -p ferge-bench --bin ferge-trace -- FILE…   # given traces
//! cargo run -p ferge-bench --bin ferge-trace -- --files --server FILE
//! ```
//!
//! Shows the route and why, the probes, the phases, throughput over time,
//! durations per file, errors, every command run on a server, and with
//! `--server` what the servers logged.

#![allow(
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a report on stdout, with rounded numbers"
)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::Parser;
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "ferge-trace", about = "Summarizes ferge transfer traces")]
struct Cli {
    /// Trace files (`.jsonl`) or folders of them. Default: the newest trace
    /// in ferge's state folder.
    traces: Vec<PathBuf>,
    /// List every file with its size, duration and method.
    #[arg(long)]
    files: bool,
    /// Print what the servers logged during a direct transfer.
    #[arg(long)]
    server: bool,
    /// Print the output of every command run on a server.
    #[arg(long)]
    commands: bool,
    /// The newest N traces instead of one.
    #[arg(long, value_name = "N")]
    last: Option<usize>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = traces(&cli)?;
    for (number, path) in paths.iter().enumerate() {
        if number > 0 {
            println!("\n{}\n", "─".repeat(78));
        }
        let events = read(path)?;
        print!("{}", report(path, &events, &cli));
    }
    Ok(())
}

/// What happened on the servers besides copying: the job key, what earlier
/// jobs left, renewals, and whether each server was left as found.
fn servers(out: &mut String, events: &[Value]) {
    let keys: Vec<&Value> = of(events, "direct.key").collect();
    let sweeps: Vec<&Value> = of(events, "direct.sweep").collect();
    let footprints: Vec<&Value> = of(events, "direct.footprint").collect();
    if keys.is_empty() && sweeps.is_empty() && footprints.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nServers");
    for key in keys {
        let zone = key["zone_s"].as_i64().map_or_else(
            || "unknown zone".to_owned(),
            |zone| format!("UTC{:+}h", zone as f64 / 3600.0),
        );
        let _ = writeln!(
            out,
            "  job key until {} ({zone}), lasting {}",
            text(&key["expiry"]),
            duration(number(&key["lifetime_s"]) * 1000.0)
        );
    }
    let renewed = of(events, "direct.renew").count();
    if renewed > 0 {
        let failed = of(events, "direct.renew")
            .filter(|renew| renew["ok"] != true)
            .count();
        let _ = writeln!(out, "  key renewed {renewed} times ({failed} failed)");
    }
    for sweep in sweeps {
        let swept: Vec<&str> = sweep["swept"]
            .as_array()
            .map(|swept| swept.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {} ({}): removed what earlier jobs left: {}",
            text(&sweep["remote"]),
            text(&sweep["side"]),
            if swept.is_empty() {
                "-".to_owned()
            } else {
                swept.join(", ")
            }
        );
        if let Some(results) = sweep["results"].as_object() {
            for (job, result) in results {
                let last = text(result).lines().last().unwrap_or_default().to_owned();
                let _ = writeln!(out, "    job {job} had ended without ferge: {last}");
            }
        }
    }
    for footprint in footprints {
        let verdict = if footprint["kept"] == true {
            "job folders kept (FERGE_DIRECT_KEEP)".to_owned()
        } else if let Some(error) = footprint["error"].as_str() {
            format!("cleaning up FAILED: {error}")
        } else if footprint["as_found"] == true {
            "left as found".to_owned()
        } else {
            let differences: Vec<&str> = footprint["differences"]
                .as_array()
                .map(|all| all.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!("NOT left as found: {}", differences.join("; "))
        };
        let _ = writeln!(
            out,
            "  {} ({}): {verdict}",
            text(&footprint["remote"]),
            text(&footprint["side"])
        );
    }
}

fn default_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|dir| dir.join("ferge").join("transfers"))
}

/// The trace files to report on, oldest first.
fn traces(cli: &Cli) -> anyhow::Result<Vec<PathBuf>> {
    let inputs = if cli.traces.is_empty() {
        vec![default_dir().context("no state folder; name a trace")?]
    } else {
        cli.traces.clone()
    };
    let mut files = Vec::new();
    let mut from_dirs = Vec::new();
    for input in inputs {
        if input.is_dir() {
            for entry in std::fs::read_dir(&input)? {
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    from_dirs.push(path);
                }
            }
        } else {
            files.push(input);
        }
    }
    from_dirs.sort();
    let keep = cli.last.unwrap_or(1);
    files.extend(from_dirs.iter().rev().take(keep).rev().cloned());
    if files.is_empty() {
        bail!("no traces found");
    }
    Ok(files)
}

fn read(path: &Path) -> anyhow::Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(number, line)| {
            serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), number + 1))
        })
        .collect()
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

fn size(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn duration(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{ms:.1} ms")
    } else if ms < 120_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{:.1} min", ms / 60_000.0)
    }
}

fn rate(bytes: f64, ms: f64) -> String {
    if ms <= 0.0 {
        return "–".to_owned();
    }
    format!("{}/s", size(bytes / (ms / 1000.0)))
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[f64], p: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

fn sparkline(values: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().fold(0.0, f64::max);
    values
        .iter()
        .map(|value| {
            if max <= 0.0 {
                BARS[0]
            } else {
                BARS[((value / max) * 7.0).round() as usize]
            }
        })
        .collect()
}

/// The events named `name`.
fn of<'a>(events: &'a [Value], name: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    events.iter().filter(move |event| event["ev"] == name)
}

fn first<'a>(events: &'a [Value], name: &'a str) -> Option<&'a Value> {
    of(events, name).next()
}

#[expect(clippy::too_many_lines, reason = "one section after the other")]
fn report(path: &Path, events: &[Value], cli: &Cli) -> String {
    let mut out = String::new();

    // Header.
    if let Some(header) = first(events, "header") {
        let _ = writeln!(
            out,
            "{}  (transfer {}, {})",
            text(&header["title"]),
            header["transfer"],
            text(&header["started"])
        );
        let _ = writeln!(
            out,
            "  {} → {}:{}",
            text(&header["source"]),
            text(&header["target"]),
            text(&header["target_dir"])
        );
        let _ = writeln!(
            out,
            "  policy {}, ferge {}",
            text(&header["route_policy"]),
            text(&header["ferge"])
        );
    }
    let _ = writeln!(out, "  trace {}", path.display());

    // Route.
    let _ = writeln!(out, "\nRoute");
    for approval in of(events, "direct.approval") {
        let _ = writeln!(
            out,
            "  asked: {} (answered in {})",
            if approval["allowed"] == true {
                "copy directly"
            } else {
                "through this computer"
            },
            duration(number(&approval["ms"]))
        );
    }
    for attempt in of(events, "direct.attempt") {
        let _ = writeln!(
            out,
            "  try {:<5} job {}  {} dials {} at {}",
            text(&attempt["mode"]),
            text(&attempt["job"]),
            text(&attempt["active"]),
            text(&attempt["passive"]),
            text(&attempt["dial"])
        );
    }
    for probe in of(events, "direct.probe") {
        let report = &probe["report"];
        let missing: Vec<&String> = report["tools"]
            .as_object()
            .map(|tools| {
                tools
                    .iter()
                    .filter(|(_, path)| text(path).is_empty())
                    .map(|(name, _)| name)
                    .collect()
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  probe {} in {} (login {}, clock {}): {}; {}{}",
            if probe["ok"] == true { "ok" } else { "FAILED" },
            duration(number(&probe["ms"])),
            report
                .get("login_ms")
                .map_or_else(|| "?".to_owned(), |ms| duration(number(ms))),
            text(&report["clock-source"]),
            text(&report["system"]),
            text(&report["ssh"]),
            if missing.is_empty() {
                String::new()
            } else {
                format!("; missing {missing:?}")
            }
        );
    }
    for unavailable in of(events, "direct.unavailable") {
        let _ = writeln!(
            out,
            "  no {}: {}",
            text(&unavailable["mode"]),
            text(&unavailable["reason"])
        );
    }
    for route in of(events, "route") {
        let _ = writeln!(
            out,
            "  → {}{}",
            text(&route["route"]),
            route["note"]
                .as_str()
                .map(|note| format!("  ({note})"))
                .unwrap_or_default()
        );
    }

    // Phases.
    let _ = writeln!(out, "\nPhases");
    let mut total_ms = 0.0;
    for phase in of(events, "phase") {
        let ms = number(&phase["ms"]);
        total_ms += ms;
        let mut line = format!("  {:<13} {:>10}", text(&phase["phase"]), duration(ms));
        if phase["phase"] == "plan" {
            let _ = write!(
                line,
                "   {} files, {} folders, {}",
                phase["files"],
                phase["dirs"],
                size(number(&phase["bytes"]))
            );
        }
        let _ = writeln!(out, "{line}");
    }
    if let Some(files) = first(events, "direct.files") {
        let _ = writeln!(
            out,
            "  (file lists: {} uploads, {} in {}; {} files alone, {} in {} batches{})",
            files["uploads"],
            size(number(&files["bytes"])),
            duration(number(&files["ms"])),
            files["alone"],
            files["batched"],
            files["batches"],
            if files["batching"] == false {
                ", batching off"
            } else {
                ""
            }
        );
    }
    let batches: Vec<f64> = of(events, "direct.batch.end")
        .filter_map(|batch| batch["ms"].as_f64())
        .collect();
    if !batches.is_empty() {
        let slowest = batches.iter().copied().fold(0.0, f64::max);
        let failed = of(events, "direct.batch.end")
            .filter(|batch| batch["code"] != 0)
            .count();
        let _ = writeln!(
            out,
            "  batches: {}, {} on average, slowest {}{}",
            batches.len(),
            duration(batches.iter().sum::<f64>() / batches.len() as f64),
            duration(slowest),
            if failed > 0 {
                format!(", {failed} failed")
            } else {
                String::new()
            }
        );
    }
    servers(&mut out, events);

    // Result.
    let end = of(events, "state").last();
    let progress = end.map(|end| &end["progress"]);
    let copy_ms = of(events, "phase")
        .filter(|phase| phase["phase"] == "copy")
        .map(|phase| number(&phase["ms"]))
        .sum::<f64>();
    let wall_ms = events.last().map_or(0.0, |event| number(&event["t"]));
    let _ = writeln!(out, "\nResult");
    if let (Some(end), Some(progress)) = (end, progress) {
        let bytes = number(&progress["bytes_done"]);
        let _ = writeln!(
            out,
            "  {}: {} of {} files ({} failed, {} skipped), {} of {}",
            text(&end["state"]),
            progress["files_done"],
            progress["files_total"],
            progress["files_failed"],
            progress["files_skipped"],
            size(bytes),
            size(number(&progress["bytes_total"]))
        );
        let _ = writeln!(
            out,
            "  copying {} at {}; all phases {}; wall clock {}",
            duration(copy_ms),
            rate(bytes, copy_ms),
            duration(total_ms),
            duration(wall_ms)
        );
    }

    // Throughput over time.
    let samples: Vec<(f64, f64)> = of(events, "progress")
        .map(|sample| (number(&sample["t"]), number(&sample["bytes_done"])))
        .collect();
    if samples.len() > 1 {
        let rates: Vec<f64> = samples
            .windows(2)
            .map(|pair| (pair[1].1 - pair[0].1) / ((pair[1].0 - pair[0].0) / 1000.0).max(0.001))
            .collect();
        let peak = rates.iter().copied().fold(0.0, f64::max);
        let shown: Vec<f64> = rates.iter().rev().take(70).rev().copied().collect();
        let _ = writeln!(
            out,
            "\nThroughput per second (peak {}/s{})",
            size(peak),
            if shown.len() < rates.len() {
                ", last 70 s"
            } else {
                ""
            }
        );
        let _ = writeln!(out, "  {}", sparkline(&shown));
    }

    // Files.
    let mut paths: HashMap<i64, String> = HashMap::new();
    for start in of(events, "file.start") {
        if let (Some(index), Some(path)) = (start["i"].as_i64(), start["path"].as_str()) {
            paths.insert(index, path.to_owned());
        }
    }
    let done: Vec<&Value> = of(events, "file.done").collect();
    let mut methods: BTreeMap<String, usize> = BTreeMap::new();
    for file in &done {
        *methods.entry(text(&file["method"]).to_owned()).or_default() += 1;
    }
    // A batched file's duration is its batch's.
    let alone = |file: &&&Value| file["method"] != "direct-batch";
    let mut durations: Vec<f64> = done
        .iter()
        .filter(alone)
        .filter_map(|file| file["ms"].as_f64())
        .collect();
    durations.sort_by(f64::total_cmp);
    let mut small: Vec<f64> = done
        .iter()
        .filter(alone)
        .filter(|file| number(&file["bytes"]) < 64.0 * 1024.0)
        .filter_map(|file| file["ms"].as_f64())
        .collect();
    small.sort_by(f64::total_cmp);
    if !done.is_empty() {
        let _ = writeln!(out, "\nFiles ({} done; {methods:?})", done.len());
        if !durations.is_empty() {
            let _ = writeln!(
                out,
                "  duration p50 {}  p90 {}  p99 {}  max {}",
                duration(percentile(&durations, 50)),
                duration(percentile(&durations, 90)),
                duration(percentile(&durations, 99)),
                duration(durations.last().copied().unwrap_or_default())
            );
        }
        if small.len() >= 5 {
            let _ = writeln!(
                out,
                "  files under 64 KiB: {} take {} each (median): the cost per file",
                small.len(),
                duration(percentile(&small, 50))
            );
        }
        let mut slowest: Vec<&&Value> = done.iter().filter(|file| file["ms"].is_number()).collect();
        slowest.sort_by(|a, b| number(&b["ms"]).total_cmp(&number(&a["ms"])));
        for file in slowest.iter().take(5) {
            let bytes = number(&file["bytes"]);
            let ms = number(&file["ms"]);
            let index = file["i"].as_i64().unwrap_or(-1);
            let _ = writeln!(
                out,
                "  slow {:>10} {:>10} {:>12}  {}",
                duration(ms),
                size(bytes),
                rate(bytes, ms),
                paths.get(&index).map_or("?", String::as_str)
            );
        }
    }
    if cli.files {
        let _ = writeln!(out, "\nEvery file");
        for file in of(events, "file.done")
            .chain(of(events, "file.error"))
            .chain(of(events, "file.skip"))
        {
            let index = file["i"].as_i64().unwrap_or(-1);
            let _ = writeln!(
                out,
                "  {:>6} {:<6} {:>10} {:>10}  {}{}",
                index,
                text(&file["ev"]).trim_start_matches("file."),
                size(number(&file["bytes"])),
                file["ms"].as_f64().map_or_else(String::new, duration),
                paths.get(&index).map_or("?", String::as_str),
                file["error"]
                    .as_str()
                    .map(|error| format!("  ({error})"))
                    .unwrap_or_default()
            );
        }
    }

    // Errors.
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for error in of(events, "file.error") {
        *errors.entry(text(&error["error"]).to_owned()).or_default() += 1;
    }
    let transfer_errors: Vec<&str> = of(events, "transfer.error")
        .map(|e| text(&e["error"]))
        .collect();
    if !errors.is_empty() || !transfer_errors.is_empty() {
        let _ = writeln!(out, "\nErrors");
        for error in transfer_errors {
            let _ = writeln!(out, "  transfer: {error}");
        }
        for (error, count) in &errors {
            let _ = writeln!(out, "  {count:>6} × {error}");
        }
    }

    // Commands on the servers.
    let commands: Vec<&Value> = of(events, "exec").collect();
    if !commands.is_empty() {
        let total: f64 = commands.iter().map(|command| number(&command["ms"])).sum();
        let failed = commands
            .iter()
            .filter(|command| command["status"] != 0)
            .count();
        let _ = writeln!(
            out,
            "\nCommands on the servers: {} in {} ({failed} not successful)",
            commands.len(),
            duration(total)
        );
        for command in &commands {
            let status = command["status"].as_u64().map_or_else(
                || text(&command["error"]).to_owned(),
                |status| status.to_string(),
            );
            let _ = writeln!(
                out,
                "  {:<7} {:>9} {:>4}  {}",
                text(&command["side"]),
                duration(number(&command["ms"])),
                status,
                command["script"].as_str().map_or_else(
                    || text(&command["command"])
                        .chars()
                        .take(90)
                        .collect::<String>(),
                    |script| format!("{} ({script})", text(&command["command"]))
                )
            );
            let stderr = text(&command["stderr"]).trim();
            if cli.commands || (command["status"] != 0 && !stderr.is_empty()) {
                for line in text(&command["stdout"]).lines().chain(stderr.lines()) {
                    let _ = writeln!(out, "            │ {line}");
                }
            }
        }
    }

    // What the servers wrote.
    let artifacts: Vec<&Value> = of(events, "artifact").collect();
    if !artifacts.is_empty() {
        let _ = writeln!(out, "\nServer files");
        for artifact in &artifacts {
            let body = text(&artifact["text"]);
            let _ = writeln!(
                out,
                "  {} {:<11} {} lines",
                text(&artifact["side"]),
                text(&artifact["name"]),
                body.lines().count()
            );
            let interesting = artifact["name"] != "status" || cli.server;
            if cli.server && interesting
                || (artifact["name"] != "status" && !body.trim().is_empty())
            {
                for line in body.lines().take(if cli.server { usize::MAX } else { 20 }) {
                    let _ = writeln!(out, "            │ {line}");
                }
            }
        }
    }
    let unknown = of(events, "direct.status.unknown").count();
    if unknown > 0 {
        let _ = writeln!(out, "\n{unknown} status lines could not be read");
    }
    out
}
