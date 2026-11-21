//! Copying the files of a plan: conflicts, partial files, resuming.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::StreamExt as _;

use super::plan::{FileStatus, Plan, PlannedFile, part_path};
use super::{Conflict, ConflictAction, Shared, Stop, Transfer, lock, names};
use crate::entry::Metadata;
use crate::error::{Error, Result};
use crate::path::RemotePath;

/// Files of one transfer copied at the same time (the queue-wide limit
/// applies on top).
const FILES_PER_TRANSFER: usize = 4;

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
                .record_error(format!("{}: {error}", file.source));
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

    // Continue a partial file only if it still matches what was confirmed.
    let resume_at = lock(&file.state).resume_at;
    let offset = match resume_at {
        Some(at) if unchanged && at > 0 => match target.stat(&part).await {
            Ok(metadata) if metadata.size >= at && at <= current.size => at,
            _ => 0,
        },
        _ => 0,
    };
    {
        let mut state = lock(&file.state);
        transfer
            .progress
            .add_bytes(i128::from(offset) - i128::from(state.counted));
        state.counted = offset;
    }

    let mut reader = source.read_from(&file.source, offset).await?;
    let mut writer = target.open_writer(&part, offset).await?;
    lock(&file.state).resume_at = Some(offset);
    while let Some(chunk) = reader.next().await {
        let chunk = chunk?;
        let len = chunk.len() as u64;
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
    let same_file = Arc::ptr_eq(&transfer.source, &transfer.target) && file.source == file.target;
    let action = if same_file {
        // Copying a file onto itself can only mean "duplicate".
        ConflictAction::KeepBoth
    } else {
        let conflict = Conflict {
            transfer: transfer.id,
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

/// Asks the transfer's resolver, unless an earlier answer applies to all.
/// Questions of one transfer are asked one at a time.
async fn resolve(transfer: &Transfer, conflict: Conflict) -> Option<ConflictAction> {
    let mut remembered = transfer.conflicts.lock().await;
    if let Some(action) = *remembered {
        return Some(action);
    }
    let decision = transfer.resolver.resolve(conflict).await?;
    if decision.apply_to_all {
        *remembered = Some(decision.action);
    }
    Some(decision.action)
}
