//! Copying the files of a plan: conflicts, partial files, resuming.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::StreamExt as _;

use super::plan::{FileStatus, Plan, PlannedFile, part_path};
use super::{Conflict, ConflictAction, ConflictKind, Shared, Stop, Transfer, lock, names};
use crate::entry::{EntryKind, Metadata};
use crate::error::{Error, Result};
use crate::path::RemotePath;
use crate::remote::Remote;

/// Files of one transfer copied at the same time (the queue-wide limit
/// applies on top).
const FILES_PER_TRANSFER: usize = 4;
/// How far a file may grow beyond its size while it is copied before the
/// copy is given up: a source that sends without end must not fill the
/// target's disk.
const GROWTH_ALLOWANCE: u64 = 64 * 1024 * 1024;

enum Outcome {
    Copied,
    Skipped,
}

/// Copies every pending file of `plan`.
pub(super) async fn copy_files(transfer: &Arc<Transfer>, shared: &Arc<Shared>, plan: &Arc<Plan>) {
    let pending: Vec<usize> = plan
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| lock(&file.state).status == FileStatus::Pending)
        .map(|(index, _)| index)
        .collect();
    futures::stream::iter(pending)
        .for_each_concurrent(FILES_PER_TRANSFER, |index| {
            let transfer = Arc::clone(transfer);
            let shared = Arc::clone(shared);
            let plan = Arc::clone(plan);
            async move {
                // The semaphore is never closed, so acquiring cannot fail.
                let Ok(_slot) = shared.slots.acquire().await else {
                    return;
                };
                let file = &plan.files[index];
                transfer.control().current = file.source.file_name().map(str::to_owned);
                let outcome = copy_file(&transfer, file).await;
                finish_file(&transfer, file, outcome);
            }
        })
        .await;
}

/// Marks a file that is still pending as failed with `message`, without
/// trying to copy it.
pub(super) fn fail_pending(transfer: &Transfer, file: &PlannedFile, message: String) {
    if lock(&file.state).status != FileStatus::Pending {
        return;
    }
    finish_file(
        transfer,
        file,
        Err(Error::Remote {
            path: file.target.clone(),
            message,
        }),
    );
}

fn finish_file(transfer: &Transfer, file: &PlannedFile, outcome: Result<Outcome>) {
    let progress = &transfer.progress;
    let mut state = lock(&file.state);
    match outcome {
        Ok(Outcome::Copied) => {
            state.status = FileStatus::Done;
            progress.files_done.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Outcome::Skipped) => {
            state.status = FileStatus::Skipped;
            // Count the skipped bytes as done so progress reaches the end.
            progress.add_bytes(i128::from(state.size) - i128::from(state.counted));
            state.counted = state.size;
            progress.files_done.fetch_add(1, Ordering::Relaxed);
            progress.files_skipped.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            state.status = FileStatus::Failed;
            progress.files_failed.fetch_add(1, Ordering::Relaxed);
            drop(state);
            transfer
                .control()
                .record_error(super::describe(&file.source, &error));
        }
    }
}

async fn copy_file(transfer: &Transfer, file: &PlannedFile) -> Result<Outcome> {
    let source = transfer.source.as_ref();
    let target = transfer.target.as_ref();

    // The source may have changed since it was planned or last attempted.
    let current = source.stat(&file.source).await?;
    if current.is_dir() {
        return Err(Error::Remote {
            path: file.source.clone(),
            message: "became a folder".into(),
        });
    }
    let unchanged = {
        let mut state = lock(&file.state);
        let unchanged = state.size == current.size && state.modified == current.modified;
        transfer
            .progress
            .adjust_total(i128::from(current.size) - i128::from(state.size));
        state.size = current.size;
        state.modified = current.modified;
        unchanged
    };

    let destination = lock(&file.state).destination.clone();
    let (destination, replace) = match destination {
        Some(destination) => destination,
        None => match choose_destination(transfer, file, &current).await? {
            Some(destination) => {
                lock(&file.state).destination = Some(destination.clone());
                destination
            }
            None => return Ok(Outcome::Skipped),
        },
    };
    let part =
        part_path(&destination).ok_or_else(|| Error::InvalidPath(destination.as_str().into()))?;

    let offset = start_partial_file(target, file, &part, unchanged, current.size).await?;
    {
        let mut state = lock(&file.state);
        transfer
            .progress
            .add_bytes(i128::from(offset) - i128::from(state.counted));
        state.counted = offset;
    }

    let mut reader = source.read_from(&file.source, offset).await?;
    let mut writer = open_partial_file(target, &part, offset).await?;
    lock(&file.state).resume_at = Some(offset);
    // A file may grow while it is read (a log, say), but not without end.
    let most = current
        .size
        .saturating_add(current.size / 2)
        .saturating_add(GROWTH_ALLOWANCE);
    let mut written = offset;
    while let Some(chunk) = reader.next().await {
        let chunk = chunk?;
        let len = chunk.len() as u64;
        written = written.saturating_add(len);
        if written > most {
            // Nothing of it is worth resuming.
            drop(writer);
            if let Err(error) = target.delete_file(&part).await {
                tracing::debug!(path = %part, %error, "could not remove the partial file");
            }
            lock(&file.state).resume_at = None;
            return Err(Error::Remote {
                path: file.source.clone(),
                message: "kept growing far beyond its size while it was copied".into(),
            });
        }
        writer.write(chunk).await?;
        let confirmed = writer.confirmed();
        let mut state = lock(&file.state);
        state.counted += len;
        state.resume_at = Some(confirmed);
        transfer.progress.add_bytes(i128::from(len));
    }
    let length = writer.finish().await?;
    {
        let mut state = lock(&file.state);
        // The file may have grown or shrunk while it was read.
        transfer
            .progress
            .adjust_total(i128::from(length) - i128::from(state.size));
        state.size = length;
        state.resume_at = Some(length);
    }

    if replace {
        target.rename_replace(&part, &destination).await?;
    } else {
        target.rename(&part, &destination).await?;
    }
    if let Some(modified) = current.modified
        && let Err(error) = target.set_modified(&destination, modified).await
    {
        tracing::debug!(path = %destination, %error, "could not keep the modification time");
    }
    Ok(Outcome::Copied)
}

/// Where to start writing the partial file `part`: where an earlier
/// attempt stopped, if the source is `unchanged` and what was written is
/// still there, otherwise at 0.
async fn start_partial_file(
    target: &dyn Remote,
    file: &PlannedFile,
    part: &RemotePath,
    unchanged: bool,
    size: u64,
) -> Result<u64> {
    // Continue only a regular file: never write through a symlink planted
    // there.
    let resume_at = lock(&file.state).resume_at;
    let offset = match resume_at {
        Some(at) if unchanged && at > 0 => match target.lstat(part).await {
            Ok(metadata)
                if metadata.kind == EntryKind::File && metadata.size >= at && at <= size =>
            {
                at
            }
            _ => 0,
        },
        _ => 0,
    };
    Ok(offset)
}

/// Opens the partial file `part` at `offset`. A new one is created
/// exclusively; if something has its name already (a partial file of an
/// earlier attempt, or a symlink someone planted), that is removed, the
/// link itself and not its target, and the file is created again. Asking
/// first would cost every file a round trip.
async fn open_partial_file(
    target: &dyn Remote,
    part: &RemotePath,
    offset: u64,
) -> Result<Box<dyn crate::remote::FileWriter>> {
    match target.open_writer(part, offset).await {
        Err(Error::AlreadyExists { .. }) if offset == 0 => {
            match target.delete_file(part).await {
                Ok(()) | Err(Error::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
            target.open_writer(part, 0).await
        }
        result => result,
    }
}

/// Where the file goes and whether that replaces an existing file; `None` to
/// skip it.
async fn choose_destination(
    transfer: &Transfer,
    file: &PlannedFile,
    source: &Metadata,
) -> Result<Option<(RemotePath, bool)>> {
    let target = transfer.target.as_ref();
    let existing = match target.stat(&file.target).await {
        Err(Error::NotFound { .. }) => return Ok(Some((file.target.clone(), false))),
        Err(error) => return Err(error),
        Ok(existing) => existing,
    };
    if existing.is_dir() {
        return Err(Error::AlreadyExists {
            path: file.target.clone(),
        });
    }
    let same_remote = Arc::ptr_eq(&transfer.source, &transfer.target)
        || (transfer.source.is_local() && transfer.target.is_local());
    let same_file = same_remote && file.source == file.target;
    let action = if same_file {
        // Copying a file onto itself can only mean "duplicate".
        ConflictAction::KeepBoth
    } else {
        let conflict = Conflict {
            transfer: transfer.id,
            kind: ConflictKind::File,
            path: file.target.clone(),
            source_size: source.size,
            source_modified: source.modified,
            existing_size: existing.size,
            existing_modified: existing.modified,
        };
        let Some(action) = resolve(transfer, conflict).await else {
            // The question was dismissed: cancel the transfer.
            transfer.stop(Stop::Cancel);
            return Ok(None);
        };
        action
    };
    match action {
        ConflictAction::Skip => Ok(None),
        ConflictAction::Overwrite => Ok(Some((file.target.clone(), true))),
        ConflictAction::KeepBoth => {
            Ok(Some((names::free_name(target, &file.target).await?, false)))
        }
    }
}

/// Asks the transfer's resolver, unless an earlier answer applies to all
/// conflicts of this kind. Questions of one transfer are asked one at a
/// time.
pub(super) async fn resolve(transfer: &Transfer, conflict: Conflict) -> Option<ConflictAction> {
    let kind = conflict.kind;
    let mut remembered = transfer.conflicts.lock().await;
    if let Some(action) = remembered.get(kind) {
        return Some(action);
    }
    // Another file's question may have stopped the transfer while this one
    // waited for its turn; do not ask again.
    if transfer.control().stop.is_some() {
        return None;
    }
    let decision = transfer.resolver.resolve(conflict).await?;
    if decision.apply_to_all {
        remembered.set(kind, decision.action);
    }
    Some(decision.action)
}
