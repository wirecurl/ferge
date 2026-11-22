//! What ferge keeps on this computer about a running direct job, so that it
//! can follow the job again after it quit: one JSON file per job in
//! [`DirectOptions::records`](crate::transfer::route::DirectOptions::records),
//! written before the job starts and removed once ferge cleaned up after it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::script::Mode;
use crate::path::RemotePath;
use crate::transfer::plan::RecordedFile;

/// The format of the files.
const VERSION: u64 = 1;

/// How ferge connected to one of the servers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedServer {
    /// The host as asked for (an `ssh_config` alias, a name or an address).
    pub alias: String,
    pub user: String,
    pub port: u16,
    /// As shown in the transfer list.
    pub label: String,
}

/// A direct job ferge started and has not cleaned up after yet.
#[derive(Clone, Debug)]
pub struct JobRecord {
    pub job: String,
    /// What the transfer list shows.
    pub title: String,
    pub source: RecordedServer,
    pub target: RecordedServer,
    pub target_dir: RemotePath,
    /// What the user copied (files and folders on the source).
    pub items: Vec<RemotePath>,
    /// When the job started (RFC 3339).
    pub started: String,
    pub(super) mode: Mode,
    pub(super) workers: usize,
    pub(super) keep: bool,
    pub(super) linger: u64,
    pub(super) lifetime_s: u64,
    pub(super) zone: Option<i32>,
    pub(super) stage: Option<String>,
    /// The state of both sides before the job, as `note`/`entry` lines.
    pub(super) before_active: String,
    pub(super) before_passive: String,
    pub(super) files: Vec<RecordedFile>,
    pub(super) batches: Vec<Vec<usize>>,
}

impl JobRecord {
    /// How many files the job copies, and how many bytes.
    pub fn totals(&self) -> (u64, u64) {
        (
            self.files.len() as u64,
            self.files.iter().map(|file| file.size).sum(),
        )
    }

    pub(super) fn to_json(&self) -> Value {
        let server = |server: &RecordedServer| {
            json!({
                "alias": server.alias,
                "user": server.user,
                "port": server.port,
                "label": server.label,
            })
        };
        json!({
            "version": VERSION,
            "job": self.job,
            "title": self.title,
            "source": server(&self.source),
            "target": server(&self.target),
            "target_dir": self.target_dir.as_str(),
            "items": self.items.iter().map(RemotePath::as_str).collect::<Vec<_>>(),
            "started": self.started,
            "mode": self.mode.as_str(),
            "workers": self.workers,
            "keep": self.keep,
            "linger": self.linger,
            "lifetime_s": self.lifetime_s,
            "zone": self.zone,
            "stage": self.stage,
            "before": { "active": self.before_active, "passive": self.before_passive },
            "files": self.files.iter().map(|file| json!([
                file.index,
                file.source.as_str(),
                file.destination.as_str(),
                file.size,
                file.replace,
                file.offset,
            ])).collect::<Vec<_>>(),
            "batches": self.batches,
        })
    }

    pub(super) fn from_json(value: &Value) -> Option<Self> {
        if value["version"].as_u64()? != VERSION {
            return None;
        }
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let server = |value: &Value| {
            Some(RecordedServer {
                alias: text(&value["alias"])?,
                user: text(&value["user"])?,
                port: u16::try_from(value["port"].as_u64()?).ok()?,
                label: text(&value["label"])?,
            })
        };
        let path = |value: &Value| RemotePath::parse(value.as_str()?).ok();
        let mut files = Vec::new();
        for file in value["files"].as_array()? {
            files.push(RecordedFile {
                index: usize::try_from(file[0].as_u64()?).ok()?,
                source: path(&file[1])?,
                destination: path(&file[2])?,
                size: file[3].as_u64()?,
                replace: file[4].as_bool()?,
                offset: file[5].as_u64()?,
            });
        }
        // Missing in records of earlier builds.
        let items = value["items"]
            .as_array()
            .map(|items| items.iter().filter_map(path).collect())
            .unwrap_or_default();
        let mut batches = Vec::new();
        for batch in value["batches"].as_array()? {
            let mut indexes = Vec::new();
            for index in batch.as_array()? {
                indexes.push(usize::try_from(index.as_u64()?).ok()?);
            }
            batches.push(indexes);
        }
        Some(Self {
            job: text(&value["job"])?,
            title: text(&value["title"])?,
            source: server(&value["source"])?,
            target: server(&value["target"])?,
            target_dir: path(&value["target_dir"])?,
            items,
            started: text(&value["started"])?,
            mode: match value["mode"].as_str()? {
                "push" => Mode::Push,
                "pull" => Mode::Pull,
                _ => return None,
            },
            workers: usize::try_from(value["workers"].as_u64()?).ok()?,
            keep: value["keep"].as_bool()?,
            linger: value["linger"].as_u64()?,
            lifetime_s: value["lifetime_s"].as_u64()?,
            zone: value["zone"]
                .as_i64()
                .and_then(|zone| i32::try_from(zone).ok()),
            stage: text(&value["stage"]),
            before_active: text(&value["before"]["active"])?,
            before_passive: text(&value["before"]["passive"])?,
            files,
            batches,
        })
    }
}

/// The file of `job` in `dir`.
pub(super) fn path(dir: &Path, job: &str) -> PathBuf {
    dir.join(format!("{job}.json"))
}

/// Writes the record, replacing an older one at once.
pub(super) async fn save(dir: &Path, record: &JobRecord) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let path = path(dir, &record.job);
    let partial = path.with_extension("json.part");
    let text = serde_json::to_vec(&record.to_json()).map_err(std::io::Error::other)?;
    tokio::fs::write(&partial, text).await?;
    tokio::fs::rename(&partial, &path).await?;
    Ok(path)
}

/// The jobs recorded in `dir`, oldest first. Files that cannot be read are
/// logged and left alone.
pub async fn recorded_jobs(dir: &Path) -> Vec<JobRecord> {
    let mut records = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return records;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let parsed = tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| JobRecord::from_json(&value));
        if let Some(record) = parsed {
            records.push(record);
        } else {
            tracing::warn!(file = %path.display(), "unreadable direct job record");
        }
    }
    records.sort_by(|a, b| a.started.cmp(&b.started));
    records
}

/// Forgets a job, e.g. one whose servers are gone for good.
pub async fn forget(dir: &Path, job: &str) {
    let _ = tokio::fs::remove_file(path(dir, job)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_survive_a_round_trip() {
        let server = |alias: &str| RecordedServer {
            alias: alias.into(),
            user: "root".into(),
            port: 22,
            label: format!("root@{alias}"),
        };
        let record = JobRecord {
            job: "j0123456789abcdef".into(),
            title: "source".into(),
            source: server("one"),
            target: server("two"),
            target_dir: RemotePath::parse("/tmp/ferge").unwrap(),
            items: vec![RemotePath::parse("/src").unwrap()],
            started: "2026-11-22T10:00:00Z".into(),
            mode: Mode::Pull,
            workers: 4,
            keep: false,
            linger: 120,
            lifetime_s: 7200,
            zone: Some(3600),
            stage: Some("/tmp/ferge/.ferge-j0123456789abcdef".into()),
            before_active: "note home 1 2 3\n".into(),
            before_passive: "note ssh -\n".into(),
            files: vec![RecordedFile {
                index: 3,
                source: RemotePath::parse("/src/a\nb").unwrap(),
                destination: RemotePath::parse("/tmp/ferge/a\nb").unwrap(),
                size: 10,
                replace: true,
                offset: 4,
            }],
            batches: vec![vec![3]],
        };
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &record).await.unwrap();
        let read = recorded_jobs(dir.path()).await;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].to_json(), record.to_json());
        assert_eq!(read[0].totals(), (1, 10));
        forget(dir.path(), &record.job).await;
        assert!(recorded_jobs(dir.path()).await.is_empty());
    }
}
