//! What a transfer copies: the folders to create and the files to copy, found
//! by listing the source recursively.

use std::sync::Mutex;
use std::time::SystemTime;

use futures::StreamExt as _;

use super::{PART_SUFFIX, lock};
use crate::entry::EntryKind;
use crate::error::{Error, Result};
use crate::path::{EntryName, RemotePath};
use crate::remote::Remote;

/// Where a file stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileStatus {
    Pending,
    Done,
    Skipped,
    Failed,
}

/// The mutable part of a [`PlannedFile`].
#[derive(Debug)]
pub(super) struct FileState {
    pub(super) status: FileStatus,
    /// Size and modification time as last seen on the source.
    pub(super) size: u64,
    pub(super) modified: Option<SystemTime>,
    /// Bytes of this file counted in the transfer's progress.
    pub(super) counted: u64,
    /// The chosen destination and whether it replaces an existing file; set
    /// once a conflict was resolved, so a resumed copy keeps the decision.
    pub(super) destination: Option<(RemotePath, bool)>,
    /// Gap-free bytes in the partial file; where a resumed copy starts.
    pub(super) resume_at: Option<u64>,
}

#[derive(Debug)]
pub(super) struct PlannedFile {
    pub(super) source: RemotePath,
    /// Where the file goes unless a conflict decides otherwise.
    pub(super) target: RemotePath,
    pub(super) state: Mutex<FileState>,
}

impl PlannedFile {
    fn new(
        source: RemotePath,
        target: RemotePath,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Self {
        Self {
            source,
            target,
            state: Mutex::new(FileState {
                status: FileStatus::Pending,
                size,
                modified,
                counted: 0,
                destination: None,
                resume_at: None,
            }),
        }
    }
}

/// Files and folders one transfer may hold at most. A server that reports
/// an endless tree (every entry a folder, ever deeper) would otherwise fill
/// the memory while the plan is made.
pub(super) const PLAN_LIMIT: usize = 1_000_000;
/// Paths longer than this are skipped; local file systems refuse them
/// anyway, and an endless tree stops here.
const PATH_LIMIT: usize = 4_096;

#[derive(Debug, Default)]
pub(super) struct Plan {
    /// Folders to create on the target, parents first.
    pub(super) dirs: Vec<RemotePath>,
    pub(super) files: Vec<PlannedFile>,
}

impl Plan {
    /// Lists `items` on `source` recursively. Returns the plan and problems
    /// that do not stop the transfer (unreadable or skipped folders).
    ///
    /// Symbolic links to files are copied as files; links to folders are
    /// skipped, which also rules out endless loops.
    pub(super) async fn scan(
        source: &dyn Remote,
        items: &[(RemotePath, EntryName)],
        target_dir: &RemotePath,
        same_remote: bool,
    ) -> Result<(Self, Vec<String>)> {
        let mut plan = Self::default();
        let mut problems = Vec::new();
        for (item, name) in items {
            // One unreadable item (e.g. a dangling link) must not stop the rest.
            let metadata = match source.stat(item).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    problems.push(super::describe(item, &error));
                    continue;
                }
            };
            let target = target_dir.join(name);
            if !metadata.is_dir() {
                plan.files.push(PlannedFile::new(
                    item.clone(),
                    target,
                    metadata.size,
                    metadata.modified,
                ));
                continue;
            }
            if same_remote && is_within(target_dir, item) {
                return Err(Error::Remote {
                    path: item.clone(),
                    message: "a folder cannot be copied into itself".into(),
                });
            }
            plan.dirs.push(target.clone());
            let mut pending = vec![(item.clone(), target)];
            while let Some((dir, target)) = pending.pop() {
                let mut listing = source.list(&dir);
                let mut entries = Vec::new();
                while let Some(chunk) = listing.next().await {
                    match chunk {
                        Ok(chunk) => entries.extend(chunk),
                        Err(error) => {
                            problems.push(super::describe(&dir, &error));
                            break;
                        }
                    }
                }
                // A stable order makes progress and conflicts predictable.
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                for entry in entries {
                    if plan.dirs.len() + plan.files.len() >= PLAN_LIMIT {
                        return Err(Error::Remote {
                            path: item.clone(),
                            message: "more than 1,000,000 files and folders, more than one \
                                      transfer holds"
                                .into(),
                        });
                    }
                    let child = dir.join(&entry.name);
                    let child_target = target.join(&entry.name);
                    if child.as_str().len() > PATH_LIMIT || child_target.as_str().len() > PATH_LIMIT
                    {
                        problems.push(format!("{child}: the path is too long, skipped"));
                        continue;
                    }
                    match entry.kind() {
                        EntryKind::Directory => {
                            plan.dirs.push(child_target.clone());
                            pending.push((child, child_target));
                        }
                        EntryKind::File => plan.files.push(PlannedFile::new(
                            child,
                            child_target,
                            entry.metadata.size,
                            entry.metadata.modified,
                        )),
                        EntryKind::Symlink => match source.stat(&child).await {
                            Ok(metadata) if metadata.is_dir() => {
                                problems.push(format!("{child}: linked folder skipped"));
                            }
                            Ok(metadata) => plan.files.push(PlannedFile::new(
                                child,
                                child_target,
                                metadata.size,
                                metadata.modified,
                            )),
                            Err(error) => problems.push(super::describe(&child, &error)),
                        },
                        EntryKind::Other => {
                            problems.push(format!("{child}: not a regular file, skipped"));
                        }
                    }
                }
            }
        }
        Ok((plan, problems))
    }

    pub(super) fn total_bytes(&self) -> u64 {
        // Sizes come from the source and may be anything.
        self.files.iter().fold(0, |total, file| {
            total.saturating_add(lock(&file.state).size)
        })
    }

    /// Creates the folders that do not exist yet. Returns problems, and the
    /// folders that are not usable: not made, or something else than a
    /// folder (a file, or a symlink, which is never followed). Nothing is
    /// to be copied below those.
    pub(super) async fn create_dirs(&self, target: &dyn Remote) -> (Vec<String>, Vec<RemotePath>) {
        let mut problems = Vec::new();
        let mut unusable: Vec<RemotePath> = Vec::new();
        for dir in &self.dirs {
            if unusable.iter().any(|bad| is_within(dir, bad)) {
                continue;
            }
            let problem = match target.lstat(dir).await {
                Ok(metadata) if metadata.kind == EntryKind::Directory => None,
                Ok(metadata) if metadata.kind == EntryKind::Symlink => Some(format!(
                    "{dir}: a symbolic link is in place of the folder; it is not followed"
                )),
                Ok(_) => Some(format!("{dir}: a file with this name exists")),
                Err(Error::NotFound { .. }) => match target.mkdir(dir).await {
                    Ok(()) => None,
                    // Made by someone else meanwhile: look at it again.
                    Err(Error::AlreadyExists { .. }) => match target.lstat(dir).await {
                        Ok(metadata) if metadata.kind == EntryKind::Directory => None,
                        _ => Some(format!("{dir}: something else appeared in its place")),
                    },
                    Err(error) => Some(super::describe(dir, &error)),
                },
                Err(error) => Some(super::describe(dir, &error)),
            };
            if let Some(problem) = problem {
                problems.push(problem);
                unusable.push(dir.clone());
            }
        }
        (problems, unusable)
    }

    /// Marks failed files as pending again. Returns how many there were.
    pub(super) fn reset_failed(&self) -> u64 {
        let mut count = 0;
        for file in &self.files {
            let mut state = lock(&file.state);
            if state.status == FileStatus::Failed {
                state.status = FileStatus::Pending;
                count += 1;
            }
        }
        count
    }

    /// Deletes the partial files of files that were not finished.
    pub(super) async fn remove_partial_files(&self, target: &dyn Remote) {
        for file in &self.files {
            let destination = {
                let state = lock(&file.state);
                if state.status == FileStatus::Done || state.resume_at.is_none() {
                    continue;
                }
                state.destination.clone()
            };
            let Some((destination, _)) = destination else {
                continue;
            };
            if let Some(part) = part_path(&destination)
                && let Err(error) = target.delete_file(&part).await
                && !matches!(error, Error::NotFound { .. })
            {
                tracing::warn!(%part, %error, "could not remove a partial file");
            }
        }
    }
}

/// `<name>.ferge-part` next to `path`.
pub(super) fn part_path(path: &RemotePath) -> Option<RemotePath> {
    let parent = path.parent()?;
    let name = EntryName::new(&format!("{}{PART_SUFFIX}", path.file_name()?)).ok()?;
    Some(parent.join(&name))
}

/// Whether `path` is `dir` or lies below it.
/// The name each item gets in the target folder. A folder that exists
/// there already is a conflict, asked about once for the folder: merge
/// into it, skip it, or keep both under a new name. Items skipped are left
/// out. `None` if the question was dismissed, which cancels the transfer.
pub(super) async fn top_level_targets(
    transfer: &super::Transfer,
    same_remote: bool,
) -> Result<Option<Vec<(RemotePath, EntryName)>>> {
    let mut targets = Vec::with_capacity(transfer.items.len());
    for item in &transfer.items {
        let name = item
            .file_name()
            .and_then(|name| EntryName::new(name).ok())
            .ok_or_else(|| Error::InvalidPath(item.as_str().into()))?;
        let target = transfer.target_dir.join(&name);
        let is_folder =
            matches!(transfer.source.stat(item).await, Ok(metadata) if metadata.is_dir());
        let existing = if is_folder {
            transfer.target.lstat(&target).await.ok()
        } else {
            None
        };
        let Some(existing) = existing.filter(|existing| existing.kind == EntryKind::Directory)
        else {
            // Files are asked about one by one while copying; anything else
            // in a folder's place is reported when folders are made.
            targets.push((item.clone(), name));
            continue;
        };
        let action = if same_remote && target == *item {
            // A folder copied onto itself can only mean "duplicate".
            super::ConflictAction::KeepBoth
        } else {
            let conflict = super::Conflict {
                transfer: transfer.id,
                kind: super::ConflictKind::Folder,
                path: target.clone(),
                source_size: 0,
                source_modified: None,
                existing_size: 0,
                existing_modified: existing.modified,
            };
            let Some(action) = super::copy::resolve(transfer, conflict).await else {
                transfer.stop(super::Stop::Cancel);
                return Ok(None);
            };
            action
        };
        match action {
            super::ConflictAction::Overwrite => targets.push((item.clone(), name)),
            super::ConflictAction::Skip => {}
            super::ConflictAction::KeepBoth => {
                let free = super::names::free_name(transfer.target.as_ref(), &target).await?;
                let name = free
                    .file_name()
                    .and_then(|name| EntryName::new(name).ok())
                    .ok_or_else(|| Error::InvalidPath(free.as_str().into()))?;
                targets.push((item.clone(), name));
            }
        }
    }
    Ok(Some(targets))
}

pub(super) fn is_within(path: &RemotePath, dir: &RemotePath) -> bool {
    path == dir
        || dir.is_root()
        || path
            .as_str()
            .strip_prefix(dir.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RemotePath {
        RemotePath::parse(text).unwrap()
    }

    #[test]
    fn detects_nested_paths() {
        assert!(is_within(&path("/a/b"), &path("/a")));
        assert!(is_within(&path("/a"), &path("/a")));
        assert!(is_within(&path("/a"), &path("/")));
        assert!(!is_within(&path("/ab"), &path("/a")));
        assert!(!is_within(&path("/a"), &path("/a/b")));
    }

    #[test]
    fn builds_part_paths() {
        assert_eq!(
            part_path(&path("/x/report.pdf")).unwrap().as_str(),
            "/x/report.pdf.ferge-part"
        );
        assert_eq!(part_path(&RemotePath::root()), None);
    }
}
