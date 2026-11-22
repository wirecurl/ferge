//! Recording what a transfer did, for debugging and for measuring.
//!
//! Every transfer writes one file of JSON lines into the trace directory:
//! a header, then events with `t` (milliseconds since the transfer was
//! queued) and `ev` (the event name). It covers the route decision with all
//! probes, every command run on a server with its output, every file with
//! timings, progress samples once a second, and at the end the files the
//! servers wrote for a direct transfer. `ferge-trace` summarizes such a file;
//! `jq` works too.
//!
//! Writing happens on a tokio task, so recording never blocks the caller.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

/// Version of the file format, in the header line.
const FORMAT: u32 = 1;

/// Longest text kept per field; longer output is cut.
pub(super) const MAX_TEXT: usize = 64 * 1024;

/// Records events of one transfer. Cheap to clone. A disabled trace drops
/// everything.
#[derive(Clone, Debug)]
pub(crate) struct Trace {
    sender: Option<mpsc::UnboundedSender<Value>>,
    started: Instant,
    path: Option<PathBuf>,
}

impl Trace {
    pub(crate) fn disabled() -> Self {
        Self {
            sender: None,
            started: Instant::now(),
            path: None,
        }
    }

    /// Starts a trace file in `dir` named after the time, the transfer id and
    /// `title`. Must be called within a tokio runtime.
    pub(crate) fn start(dir: &Path, id: u64, title: &str, header: Value) -> Self {
        let now = jiff::Timestamp::now();
        let stamp = now.strftime("%Y%m%d-%H%M%S");
        // Several queues (or ferge processes) may start a transfer in the same
        // second under the same number.
        let mut unique = [0u8; 3];
        let _ = getrandom::fill(&mut unique);
        let unique = unique.iter().fold(String::new(), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        });
        let path = dir.join(format!("{stamp}-{id}-{}-{unique}.jsonl", slug(title)));
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut line = json!({
            "ev": "header",
            "format": FORMAT,
            "ferge": env!("CARGO_PKG_VERSION"),
            "started": now.to_string(),
            "transfer": id,
            "title": title,
        });
        merge(&mut line, header);
        let _ = sender.send(line);
        tokio::spawn(write_task(dir.to_path_buf(), path.clone(), receiver));
        tracing::info!(path = %path.display(), "recording a transfer trace");
        Self {
            sender: Some(sender),
            started: Instant::now(),
            path: Some(path),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.sender.is_some()
    }

    /// Where the trace is written.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Milliseconds since the transfer was queued.
    pub(crate) fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    /// Records `event` with the fields of `fields` (a JSON object).
    pub(crate) fn event(&self, event: &str, fields: Value) {
        let Some(sender) = &self.sender else {
            return;
        };
        let t = (self.elapsed_ms() * 1000.0).round() / 1000.0;
        let mut line = json!({ "t": t, "ev": event });
        merge(&mut line, fields);
        let _ = sender.send(line);
    }
}

/// Deletes all but the newest `keep` trace files in `dir`. File names start
/// with the time, so their order is their age.
pub async fn prune(dir: PathBuf, keep: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return;
    };
    let mut traces = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".jsonl") {
            traces.push(entry.path());
        }
    }
    traces.sort();
    let excess = traces.len().saturating_sub(keep);
    for path in &traces[..excess] {
        if let Err(error) = tokio::fs::remove_file(path).await {
            tracing::debug!(path = %path.display(), %error, "could not remove an old trace");
        }
    }
    if excess > 0 {
        tracing::debug!(removed = excess, "removed old transfer traces");
    }
}

/// Copies the members of `extra` into `line`.
fn merge(line: &mut Value, extra: Value) {
    if let (Value::Object(line), Value::Object(extra)) = (line, extra) {
        line.extend(extra);
    }
}

/// `text` cut to [`MAX_TEXT`] bytes on a character boundary.
pub(crate) fn cut(text: &str) -> Value {
    if text.len() <= MAX_TEXT {
        return Value::String(text.to_owned());
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(format!(
        "{}… ({} bytes cut)",
        &text[..end],
        text.len() - end
    ))
}

/// A file-name-safe short form of `title`.
fn slug(title: &str) -> String {
    let slug: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "transfer".to_owned()
    } else {
        slug.to_owned()
    }
}

async fn write_task(dir: PathBuf, path: PathBuf, mut receiver: mpsc::UnboundedReceiver<Value>) {
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(dir = %dir.display(), %error, "cannot create the trace directory");
        return;
    }
    let file = match tokio::fs::File::create(&path).await {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot create a trace file");
            return;
        }
    };
    let mut writer = tokio::io::BufWriter::new(file);
    let mut buffer = Vec::new();
    while let Some(line) = receiver.recv().await {
        buffer.clear();
        if serde_json::to_writer(&mut buffer, &line).is_err() {
            continue;
        }
        buffer.push(b'\n');
        if let Err(error) = writer.write_all(&buffer).await {
            tracing::warn!(path = %path.display(), %error, "writing a trace failed");
            return;
        }
        // Flush whenever the queue is drained, so a crash loses little.
        if receiver.is_empty() && writer.flush().await.is_err() {
            return;
        }
    }
    let _ = writer.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_file_name_safe() {
        assert_eq!(slug("my report.pdf"), "my-report-pdf");
        assert_eq!(slug("../.."), "transfer");
        assert_eq!(slug("Ünïcode ok"), "Ünïcode-ok");
    }

    #[test]
    fn cuts_long_text_on_char_boundaries() {
        let long = "ä".repeat(MAX_TEXT);
        let Value::String(cut) = cut(&long) else {
            panic!("string");
        };
        assert!(cut.len() < long.len());
        assert!(cut.ends_with("bytes cut)"));
    }

    #[tokio::test]
    async fn writes_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let trace = Trace::start(dir.path(), 7, "a b", json!({ "source": "x" }));
        trace.event("file.done", json!({ "i": 1, "bytes": 10 }));
        let path = trace.path().unwrap().to_path_buf();
        drop(trace);
        // The writer finishes once every sender is gone.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let text = loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.lines().count() == 2 || Instant::now() > deadline {
                break text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["ev"], "header");
        assert_eq!(lines[0]["source"], "x");
        assert_eq!(lines[1]["ev"], "file.done");
        assert_eq!(lines[1]["bytes"], 10);
    }
}
