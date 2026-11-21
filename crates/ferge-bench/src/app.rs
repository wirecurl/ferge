//! Runs the Ferge binary in its benchmark modes and parses what it reports.
//!
//! The app writes `ferge-bench: …` markers to stderr; with `ZED_MEASUREMENTS=1`
//! GPUI adds a `frame duration: …` line for every drawn frame. Both go to the
//! same stream, so their order is the order of events on the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;

const MARKER: &str = "ferge-bench: ";
const FRAME: &str = "frame duration: ";

/// Where to find the app and how to reach the test server.
#[derive(Clone, Debug)]
pub(crate) struct AppUnderTest {
    pub(crate) binary: PathBuf,
    pub(crate) ssh_config: PathBuf,
    pub(crate) target: String,
}

/// Cold start timings of one run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Startup {
    /// From spawning the process until the ready marker was read.
    pub(crate) wall: Duration,
    /// From `main` until the first frame was presented, as seen by the app.
    pub(crate) in_app: Duration,
}

impl AppUnderTest {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Starts the app with an empty window and waits for its first frame.
    pub(crate) async fn startup(&self) -> anyhow::Result<Startup> {
        let mut command = self.command();
        command.args(["--bench", "startup"]);
        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", self.binary.display()))?;
        let stderr = child.stderr.take().context("no stderr")?;
        let mut lines = BufReader::new(stderr).lines();
        let read = async {
            while let Some(line) = lines.next_line().await? {
                if let Some(micros) = line.strip_prefix("ferge-bench: ready ") {
                    let wall = started.elapsed();
                    let in_app = Duration::from_micros(micros.trim().parse()?);
                    return Ok(Startup { wall, in_app });
                }
            }
            bail!("the app exited without reporting readiness")
        };
        let startup = tokio::time::timeout(Duration::from_secs(30), read)
            .await
            .context("startup timed out")??;
        // Drain the rest so the app is never blocked on a full pipe.
        let drain = async {
            while lines.next_line().await?.is_some() {}
            anyhow::Ok(())
        };
        let _ = tokio::time::timeout(Duration::from_secs(10), drain).await;
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await??;
        ensure!(status.success(), "the app exited with {status}");
        Ok(startup)
    }

    /// Runs the scripted interaction scenario against the test server.
    pub(crate) async fn interaction(&self) -> anyhow::Result<Interaction> {
        let mut command = self.command();
        command
            .env("ZED_MEASUREMENTS", "1")
            .args(["--bench", "interaction", "-F"])
            .arg(&self.ssh_config)
            .arg(&self.target);
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", self.binary.display()))?;
        let stderr = child.stderr.take().context("no stderr")?;
        let mut lines = BufReader::new(stderr).lines();
        let mut collected = Vec::new();
        let read = async {
            while let Some(line) = lines.next_line().await? {
                collected.push(line);
            }
            anyhow::Ok(())
        };
        tokio::time::timeout(Duration::from_secs(300), read)
            .await
            .context("the interaction scenario timed out")??;
        let status = child.wait().await?;
        ensure!(status.success(), "the app exited with {status}");
        Interaction::parse(&collected)
    }
}

/// Everything the interaction scenario reported.
#[derive(Debug, Default)]
pub(crate) struct Interaction {
    pub(crate) entries: usize,
    pub(crate) first_rows: Duration,
    pub(crate) listing_complete: Duration,
    /// All frames drawn during each phase.
    pub(crate) frames: HashMap<String, Vec<Duration>>,
    /// The frame drawn right after each input, per phase.
    pub(crate) input_frames: HashMap<String, Vec<Duration>>,
    /// Time from an input until the browser settled, per phase.
    pub(crate) settled: HashMap<String, Duration>,
}

impl Interaction {
    fn parse(lines: &[String]) -> anyhow::Result<Self> {
        let mut result = Self::default();
        let mut phase: Option<String> = None;
        let mut awaiting_input_frame = false;
        let mut done = false;
        for line in lines {
            if let Some(value) = line.strip_prefix(FRAME) {
                let duration = parse_duration(value)?;
                if let Some(phase) = &phase {
                    result
                        .frames
                        .entry(phase.clone())
                        .or_default()
                        .push(duration);
                    if awaiting_input_frame {
                        result
                            .input_frames
                            .entry(phase.clone())
                            .or_default()
                            .push(duration);
                    }
                }
                awaiting_input_frame = false;
                continue;
            }
            let Some(marker) = line.strip_prefix(MARKER) else {
                continue;
            };
            let mut words = marker.split_whitespace();
            match words.next() {
                Some("input") => awaiting_input_frame = true,
                Some("phase") => phase = words.next().map(str::to_owned),
                Some("listing") => {
                    let fields = key_values(words);
                    result.entries = field(&fields, "entries")?;
                    result.first_rows = Duration::from_micros(field(&fields, "first_rows_us")?);
                    result.listing_complete = Duration::from_micros(field(&fields, "complete_us")?);
                }
                Some("settled") => {
                    let name = words.next().context("settled without a name")?.to_owned();
                    let fields = key_values(words);
                    result
                        .settled
                        .insert(name, Duration::from_micros(field(&fields, "us")?));
                }
                Some("error") => bail!("the scenario failed: {marker}"),
                Some("done") => done = true,
                _ => {}
            }
        }
        ensure!(done, "the scenario did not finish");
        Ok(result)
    }

    /// Frames of all phases whose name starts with `prefix`.
    pub(crate) fn frames_of(&self, prefixes: &[&str]) -> Vec<Duration> {
        collect(&self.frames, prefixes)
    }

    /// Input frames of all phases whose name starts with one of `prefixes`.
    pub(crate) fn input_frames_of(&self, prefixes: &[&str]) -> Vec<Duration> {
        collect(&self.input_frames, prefixes)
    }
}

fn collect(map: &HashMap<String, Vec<Duration>>, prefixes: &[&str]) -> Vec<Duration> {
    map.iter()
        .filter(|(phase, _)| prefixes.iter().any(|prefix| phase.starts_with(prefix)))
        .flat_map(|(_, frames)| frames.iter().copied())
        .collect()
}

fn key_values<'a>(words: impl Iterator<Item = &'a str>) -> HashMap<&'a str, &'a str> {
    words.filter_map(|word| word.split_once('=')).collect()
}

fn field<T: std::str::FromStr>(fields: &HashMap<&str, &str>, name: &str) -> anyhow::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    Ok(fields
        .get(name)
        .with_context(|| format!("missing field {name}"))?
        .parse()?)
}

/// Parses `Duration`'s `Debug` output such as `1.25ms`, `980.5µs` or `2s`.
fn parse_duration(text: &str) -> anyhow::Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .with_context(|| format!("no unit in {text:?}"))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse()?;
    let seconds = match unit {
        "s" => value,
        "ms" => value / 1e3,
        "µs" | "us" => value / 1e6,
        "ns" => value / 1e9,
        other => bail!("unknown unit {other:?}"),
    };
    Ok(Duration::from_secs_f64(seconds))
}

/// The release binary next to this harness, if it was built.
pub(crate) fn default_binary(workspace: &Path) -> PathBuf {
    workspace.join("target/release/ferge")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(
            parse_duration("1.5ms").unwrap(),
            Duration::from_micros(1500)
        );
        assert_eq!(parse_duration("250µs").unwrap(), Duration::from_micros(250));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert!(parse_duration("12").is_err());
        assert!(parse_duration("1.0h").is_err());
    }

    #[test]
    fn attributes_frames_to_inputs_and_phases() {
        let lines: Vec<String> = [
            "frame duration: 9ms",
            "ferge-bench: listing entries=10 first_rows_us=100 complete_us=900",
            "ferge-bench: phase keys-down",
            "frame duration: 1ms",
            "ferge-bench: input",
            "frame duration: 2ms",
            "frame duration: 3ms",
            "ferge-bench: phase sort-size",
            "ferge-bench: input",
            "frame duration: 4ms",
            "ferge-bench: settled sort-size us=1500",
            "ferge-bench: done",
        ]
        .map(String::from)
        .to_vec();
        let parsed = Interaction::parse(&lines).unwrap();
        assert_eq!(parsed.entries, 10);
        assert_eq!(parsed.first_rows, Duration::from_micros(100));
        assert_eq!(parsed.frames["keys-down"].len(), 3);
        assert_eq!(parsed.input_frames["keys-down"], [Duration::from_millis(2)]);
        assert_eq!(
            parsed.input_frames_of(&["sort"]),
            [Duration::from_millis(4)]
        );
        assert_eq!(parsed.settled["sort-size"], Duration::from_micros(1500));
    }

    #[test]
    fn requires_completion_and_reports_errors() {
        assert!(Interaction::parse(&["ferge-bench: phase x".to_owned()]).is_err());
        let failed = [
            "ferge-bench: error boom".to_owned(),
            "ferge-bench: done".to_owned(),
        ];
        assert!(Interaction::parse(&failed).is_err());
    }
}
