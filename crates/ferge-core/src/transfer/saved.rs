//! Transfers that need ferge to go on (through this computer, or within one
//! server) are saved, so that one that stopped because ferge quit comes
//! back after a restart, paused, and resumes where it stopped.
//!
//! In [`DirectOptions::records`](super::route::DirectOptions::records), each
//! transfer has `<key>.json`, its plan as made, and `<key>.journal`, one
//! line per event since: a conflict decided, a file settled, the bytes of a
//! file confirmed on the target. Both go once the transfer completed or was
//! cancelled or removed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use futures::channel::mpsc;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use super::direct::RecordedServer;
use super::lock;
use super::plan::{FileStatus, Plan, SavedFile};
use super::route::Route;
use crate::destination::{Destination, SftpTarget};
use crate::path::RemotePath;
use crate::remote::Remote;

/// The format of the files.
const VERSION: u64 = 1;

/// How far a file's confirmed bytes may run ahead of the journal.
const PROGRESS_STEP: u64 = 8 * 1024 * 1024;

/// One side of a saved transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavedSide {
    /// This computer.
    Local { label: String },
    /// A server, the way it was connected.
    Server(RecordedServer),
}

impl SavedSide {
    /// The side of `remote`, if it can be connected to again.
    pub(super) fn of(remote: &dyn Remote) -> Option<Self> {
        if remote.is_local() {
            return Some(Self::Local {
                label: remote.label().to_owned(),
            });
        }
        let endpoint = remote.server_side()?.endpoint();
        Some(Self::Server(RecordedServer {
            alias: endpoint.alias.clone(),
            user: endpoint.user.clone(),
            port: endpoint.port,
            label: remote.label().to_owned(),
        }))
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Local { label } => label,
            Self::Server(server) => &server.label,
        }
    }

    /// Where to connect to reach this side (no start folder).
    pub fn destination(&self) -> Option<Destination> {
        match self {
            Self::Local { .. } => Some(Destination::Local { path: None }),
            Self::Server(server) => {
                let mut target = SftpTarget::new(&server.alias).ok()?;
                target.user = Some(server.user.clone());
                target.port = Some(server.port);
                Some(Destination::Sftp(target))
            }
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Local { label } => json!({ "local": true, "label": label }),
            Self::Server(server) => json!({
                "alias": server.alias,
                "user": server.user,
                "port": server.port,
                "label": server.label,
            }),
        }
    }

    fn from_json(value: &Value) -> Option<Self> {
        let label = value["label"].as_str()?.to_owned();
        if value["local"].as_bool() == Some(true) {
            return Some(Self::Local { label });
        }
        Some(Self::Server(RecordedServer {
            alias: value["alias"].as_str()?.to_owned(),
            user: value["user"].as_str()?.to_owned(),
            port: u16::try_from(value["port"].as_u64()?).ok()?,
            label,
        }))
    }
}

/// A transfer saved on this computer, as it stood when ferge last ran.
#[derive(Debug)]
pub struct SavedTransfer {
    pub key: String,
    pub title: String,
    pub source: SavedSide,
    pub target: SavedSide,
    pub target_dir: RemotePath,
    pub items: Vec<RemotePath>,
    /// When it was saved (RFC 3339).
    pub saved: String,
    pub(super) route: Route,
    pub(super) dirs: Vec<RemotePath>,
    pub(super) files: Vec<SavedFile>,
}

impl SavedTransfer {
    /// The plan as it stands after the journal.
    pub(super) fn plan(&self) -> Plan {
        Plan::restored(self.dirs.clone(), &self.files)
    }
}

/// What a transfer writes to its saved files.
#[derive(Debug)]
enum Line {
    Text(String),
    /// Removes both files and ends.
    Forget,
}

/// The journal of a saved transfer: lines go to a background task that
/// appends them, so writing never waits for the disk.
#[derive(Debug, Default)]
pub(super) struct Journal {
    sender: Mutex<Option<mpsc::UnboundedSender<Line>>>,
    /// The confirmed bytes last journaled per file.
    progress: Mutex<HashMap<usize, u64>>,
}

impl Journal {
    pub(super) fn is_open(&self) -> bool {
        lock(&self.sender).is_some()
    }

    /// Saves `plan` of `transfer` as `key` in `dir` and starts the journal.
    pub(super) fn start(&self, dir: &Path, key: &str, record: Value) {
        let (sender, receiver) = mpsc::unbounded();
        let paths = paths(dir, key);
        tokio::spawn(write(paths, Some(record), receiver));
        *lock(&self.sender) = Some(sender);
    }

    /// Goes on with the journal of a restored transfer.
    pub(super) fn resume(&self, dir: &Path, key: &str) {
        let (sender, receiver) = mpsc::unbounded();
        tokio::spawn(write(paths(dir, key), None, receiver));
        *lock(&self.sender) = Some(sender);
    }

    fn send(&self, line: String) {
        if let Some(sender) = lock(&self.sender).as_ref() {
            let _ = sender.unbounded_send(Line::Text(line));
        }
    }

    /// A conflict was decided: file `index` goes to `destination`.
    pub(super) fn decided(&self, index: usize, destination: &RemotePath, replace: bool) {
        self.send(format!(
            "decide {index} {} {}",
            u8::from(replace),
            Value::from(destination.as_str())
        ));
    }

    /// File `index` has `bytes` confirmed on the target; journaled every few
    /// megabytes.
    pub(super) fn progressed(&self, index: usize, bytes: u64) {
        if !self.is_open() {
            return;
        }
        let due = {
            let mut progress = lock(&self.progress);
            let last = progress.entry(index).or_insert(0);
            let due = bytes >= *last + PROGRESS_STEP;
            if due {
                *last = bytes;
            }
            due
        };
        if due {
            self.send(format!("at {index} {bytes}"));
        }
    }

    /// File `index` settled (or not, after a lost connection).
    pub(super) fn settled(&self, index: usize, status: FileStatus, resume_at: Option<u64>) {
        lock(&self.progress).remove(&index);
        match status {
            FileStatus::Done => self.send(format!("done {index}")),
            FileStatus::Skipped => self.send(format!("skip {index}")),
            FileStatus::Failed => self.send(format!("fail {index}")),
            FileStatus::Pending => {
                if let Some(bytes) = resume_at {
                    self.send(format!("at {index} {bytes}"));
                }
            }
        }
    }

    /// Removes the saved transfer.
    pub(super) fn forget(&self) {
        if let Some(sender) = lock(&self.sender).take() {
            let _ = sender.unbounded_send(Line::Forget);
        }
    }
}

fn paths(dir: &Path, key: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{key}.json")),
        dir.join(format!("{key}.journal")),
    )
}

/// The writer task: saves the record (if new), then appends lines.
async fn write(
    (record_path, journal_path): (PathBuf, PathBuf),
    record: Option<Value>,
    mut lines: mpsc::UnboundedReceiver<Line>,
) {
    let result = async {
        if let Some(record) = record {
            if let Some(dir) = record_path.parent() {
                tokio::fs::create_dir_all(dir).await?;
            }
            let partial = record_path.with_extension("json.part");
            tokio::fs::write(&partial, serde_json::to_vec(&record)?).await?;
            tokio::fs::rename(&partial, &record_path).await?;
        }
        let mut journal = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)
            .await?;
        while let Some(line) = lines.next().await {
            let mut text = Vec::new();
            let mut forget = false;
            let mut next = Some(line);
            // Whatever else is waiting goes in the same write.
            while let Some(line) = next.take() {
                match line {
                    Line::Text(line) => {
                        text.extend_from_slice(line.as_bytes());
                        text.push(b'\n');
                        next = lines.try_recv().ok();
                    }
                    Line::Forget => forget = true,
                }
            }
            if forget {
                drop(journal);
                let _ = tokio::fs::remove_file(&journal_path).await;
                let _ = tokio::fs::remove_file(&record_path).await;
                return Ok(());
            }
            journal.write_all(&text).await?;
            journal.flush().await?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(file = %journal_path.display(), %error, "cannot save a transfer");
    }
}

/// A new key for a saved transfer.
pub(super) fn new_key() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().fold(String::from("t"), |mut key, byte| {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
        key
    })
}

/// The record of a transfer about to copy `plan`.
pub(super) fn record(
    key: &str,
    title: &str,
    sides: (&SavedSide, &SavedSide),
    target_dir: &RemotePath,
    items: &[RemotePath],
    route: Route,
    plan: &Plan,
) -> Value {
    let time = |time: Option<SystemTime>| {
        time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| json!([since.as_secs(), since.subsec_nanos()]))
    };
    let files: Vec<Value> = plan
        .files
        .iter()
        .map(|file| {
            let state = lock(&file.state);
            json!([
                file.source.as_str(),
                file.target.as_str(),
                state.size,
                time(state.modified),
                match state.status {
                    FileStatus::Pending => "pending",
                    FileStatus::Done => "done",
                    FileStatus::Skipped => "skip",
                    FileStatus::Failed => "fail",
                },
                state
                    .destination
                    .as_ref()
                    .map(|(path, replace)| json!([path.as_str(), replace])),
                state.resume_at,
            ])
        })
        .collect();
    json!({
        "version": VERSION,
        "key": key,
        "title": title,
        "saved": jiff::Timestamp::now().to_string(),
        "source": sides.0.to_json(),
        "target": sides.1.to_json(),
        "target_dir": target_dir.as_str(),
        "items": items.iter().map(RemotePath::as_str).collect::<Vec<_>>(),
        "route": route.as_str(),
        "dirs": plan.dirs.iter().map(RemotePath::as_str).collect::<Vec<_>>(),
        "files": files,
    })
}

fn parse(record: &Value, journal: &str) -> Option<SavedTransfer> {
    if record["version"].as_u64()? != VERSION {
        return None;
    }
    let path = |value: &Value| RemotePath::parse(value.as_str()?).ok();
    let paths =
        |value: &Value| -> Option<Vec<RemotePath>> { value.as_array()?.iter().map(path).collect() };
    let mut files = Vec::new();
    for file in record["files"].as_array()? {
        let modified = file[3].as_array().and_then(|time| {
            Some(
                UNIX_EPOCH
                    + Duration::new(
                        time.first()?.as_u64()?,
                        u32::try_from(time.get(1)?.as_u64()?).ok()?,
                    ),
            )
        });
        files.push(SavedFile {
            source: path(&file[0])?,
            target: path(&file[1])?,
            size: file[2].as_u64()?,
            modified,
            status: match file[4].as_str()? {
                "done" => FileStatus::Done,
                "skip" => FileStatus::Skipped,
                "fail" => FileStatus::Failed,
                _ => FileStatus::Pending,
            },
            destination: file[5]
                .as_array()
                .and_then(|pair| Some((path(pair.first()?)?, pair.get(1)?.as_bool()?))),
            resume_at: file[6].as_u64(),
        });
    }
    for line in journal.lines() {
        let mut words = line.splitn(4, ' ');
        let (Some(kind), Some(index)) = (words.next(), words.next()) else {
            continue;
        };
        let Some(file) = index
            .parse::<usize>()
            .ok()
            .and_then(|index| files.get_mut(index))
        else {
            continue;
        };
        match kind {
            "done" => file.status = FileStatus::Done,
            "skip" => file.status = FileStatus::Skipped,
            "fail" => file.status = FileStatus::Failed,
            "at" => file.resume_at = words.next().and_then(|bytes| bytes.parse().ok()),
            "decide" => {
                let replace = words.next() == Some("1");
                if let Some(destination) = words
                    .next()
                    .and_then(|text| serde_json::from_str::<String>(text).ok())
                    .and_then(|text| RemotePath::parse(&text).ok())
                {
                    file.destination = Some((destination, replace));
                }
            }
            _ => {}
        }
    }
    Some(SavedTransfer {
        key: record["key"].as_str()?.to_owned(),
        title: record["title"].as_str()?.to_owned(),
        source: SavedSide::from_json(&record["source"])?,
        target: SavedSide::from_json(&record["target"])?,
        target_dir: path(&record["target_dir"])?,
        items: paths(&record["items"])?,
        saved: record["saved"].as_str()?.to_owned(),
        route: match record["route"].as_str()? {
            "server" => Route::Server,
            _ => Route::Relay,
        },
        dirs: paths(&record["dirs"])?,
        files,
    })
}

/// The transfers saved in `dir`, oldest first. Direct jobs recorded there
/// are not among them (see [`direct::recorded_jobs`](super::direct::recorded_jobs)).
pub async fn saved_transfers(dir: &Path) -> Vec<SavedTransfer> {
    let mut saved = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return saved;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_transfer = path
            .extension()
            .is_some_and(|extension| extension == "json")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('t'));
        if !is_transfer {
            continue;
        }
        let record = tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let journal = tokio::fs::read_to_string(path.with_extension("journal"))
            .await
            .unwrap_or_default();
        if let Some(transfer) = record.as_ref().and_then(|record| parse(record, &journal)) {
            saved.push(transfer);
        } else {
            tracing::warn!(file = %path.display(), "unreadable saved transfer");
        }
    }
    saved.sort_by(|a, b| a.saved.cmp(&b.saved));
    saved
}

/// Removes a saved transfer, e.g. one whose servers are gone for good.
pub async fn forget_saved(dir: &Path, key: &str) {
    let (record, journal) = paths(dir, key);
    let _ = tokio::fs::remove_file(journal).await;
    let _ = tokio::fs::remove_file(record).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RemotePath {
        RemotePath::parse(text).unwrap()
    }

    #[tokio::test]
    async fn a_saved_transfer_comes_back_as_it_stood() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            SavedFile {
                source: path("/src/a"),
                target: path("/dst/a"),
                size: 10,
                modified: Some(UNIX_EPOCH + Duration::new(1_600_000_000, 5)),
                status: FileStatus::Pending,
                destination: None,
                resume_at: None,
            },
            SavedFile {
                source: path("/src/b\nc"),
                target: path("/dst/b\nc"),
                size: 30_000_000,
                modified: None,
                status: FileStatus::Pending,
                destination: None,
                resume_at: None,
            },
        ];
        let plan = Plan::restored(vec![path("/dst")], &files);
        let sides = (
            SavedSide::Local {
                label: "This computer".into(),
            },
            SavedSide::Server(RecordedServer {
                alias: "vm".into(),
                user: "root".into(),
                port: 22,
                label: "root@vm".into(),
            }),
        );
        let record = record(
            "t01",
            "src",
            (&sides.0, &sides.1),
            &path("/dst"),
            &[path("/src")],
            Route::Relay,
            &plan,
        );
        let journal = Journal::default();
        journal.start(dir.path(), "t01", record);
        journal.decided(1, &path("/dst/b\nc (2)"), false);
        journal.settled(0, FileStatus::Done, Some(10));
        journal.progressed(1, 9 * 1024 * 1024);
        journal.progressed(1, 10 * 1024 * 1024); // too soon to journal
        tokio::time::sleep(Duration::from_millis(100)).await;

        let saved = saved_transfers(dir.path()).await;
        assert_eq!(saved.len(), 1);
        let saved = &saved[0];
        assert_eq!((saved.source.clone(), saved.target.clone()), sides);
        assert_eq!(saved.files[0].status, FileStatus::Done);
        assert_eq!(saved.files[0].modified, files[0].modified);
        assert_eq!(saved.files[1].status, FileStatus::Pending);
        assert_eq!(saved.files[1].resume_at, Some(9 * 1024 * 1024));
        assert_eq!(
            saved.files[1].destination,
            Some((path("/dst/b\nc (2)"), false))
        );
        assert_eq!(
            saved.target.destination(),
            Some(Destination::parse("sftp://root@vm:22").unwrap())
        );

        journal.forget();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(saved_transfers(dir.path()).await.is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
