//! Copying the files of a plan through this computer or within one server:
//! conflicts, partial files, resuming.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use futures::StreamExt as _;
use serde_json::json;

use super::plan::{FileStatus, Plan, PlannedFile, part_path};
use super::route::Route;
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

pub(super) enum Outcome {
    /// Copied; how (`relay`, `copy-data`, `cp`, `direct`).
    Copied(&'static str),
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
                let file = &plan.files[index];
                // Before the slot: a file that waits for another transfer
                // must not keep others from being copied.
                transfer.claim(&shared, index, &file.target).await;
                // The semaphore is never closed, so acquiring cannot fail.
                let Ok(_slot) = shared.slots.acquire().await else {
                    return;
                };
                transfer.control().current = file.source.file_name().map(str::to_owned);
                let started = Instant::now();
                if transfer.trace.is_enabled() {
                    let size = lock(&file.state).size;
                    transfer.trace.event(
                        "file.start",
                        json!({ "i": index, "path": file.source.as_str(), "size": size }),
                    );
                }
                let outcome = copy_file(&transfer, index, file).await;
                trace_outcome(&transfer, index, file, &outcome, started);
                finish_file(&transfer, file, outcome);
                let (status, resume_at) = {
                    let state = lock(&file.state);
                    (state.status, state.resume_at)
                };
                if status != FileStatus::Pending {
                    transfer.release(index);
                }
                transfer.journal.settled(index, status, resume_at);
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

fn trace_outcome(
    transfer: &Transfer,
    index: usize,
    file: &PlannedFile,
    outcome: &Result<Outcome>,
    started: Instant,
) {
    if !transfer.trace.is_enabled() {
        return;
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    match outcome {
        Ok(Outcome::Copied(method)) => {
            let bytes = lock(&file.state).size;
            transfer.trace.event(
                "file.done",
                json!({ "i": index, "bytes": bytes, "ms": ms, "method": method }),
            );
        }
        Ok(Outcome::Skipped) => transfer
            .trace
            .event("file.skip", json!({ "i": index, "ms": ms })),
        Err(error) => transfer.trace.event(
            "file.error",
            json!({ "i": index, "ms": ms, "error": error.to_string() }),
        ),
    }
}

/// Records how a file ended and updates the transfer's counters.
pub(super) fn finish_file(transfer: &Transfer, file: &PlannedFile, outcome: Result<Outcome>) {
    let progress = &transfer.progress;
    let mut state = lock(&file.state);
    match outcome {
        Ok(Outcome::Copied(_)) => {
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
        Err(error) if error.is_connection_lost() => {
            // Not the file's fault: it waits for a new connection.
            drop(state);
            transfer.stop(Stop::Lost);
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

async fn copy_file(transfer: &Transfer, index: usize, file: &PlannedFile) -> Result<Outcome> {
    let (source, target) = (transfer.source(), transfer.target());
    let (source, target) = (source.as_ref(), target.as_ref());

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
                transfer
                    .journal
                    .decided(index, &destination.0, destination.1);
                destination
            }
            None => return Ok(Outcome::Skipped),
        },
    };
    let capabilities = target.capabilities();
    let method = if capabilities.atomic_writes {
        // The file appears only once it is complete: no partial file, and
        // nothing to resume, so the copy starts at the beginning.
        resume_offset(transfer, index, file, None).await;
        let sink = Sink::Destination {
            path: &destination,
            replace,
        };
        let length = relay(transfer, index, file, sink, current.size).await?;
        settle_length(transfer, file, length);
        "relay"
    } else {
        let part = part_path(&destination)
            .ok_or_else(|| Error::InvalidPath(destination.as_str().into()))?;
        let resumable = (unchanged && capabilities.resume_writes).then_some((&part, current.size));
        let offset = resume_offset(transfer, index, file, resumable).await;
        let (length, method) =
            write_part(transfer, index, file, &part, offset, current.size).await?;
        settle_length(transfer, file, length);
        if replace {
            target.rename_replace(&part, &destination).await?;
        } else {
            target.rename(&part, &destination).await?;
        }
        method
    };
    if capabilities.set_modified
        && let Some(modified) = current.modified
        && let Err(error) = target.set_modified(&destination, modified).await
    {
        tracing::debug!(path = %destination, %error, "could not keep the modification time");
    }
    Ok(Outcome::Copied(method))
}

/// Records the length `file` ended up with: it may have grown or shrunk
/// while it was read.
fn settle_length(transfer: &Transfer, file: &PlannedFile, length: u64) {
    let mut state = lock(&file.state);
    transfer
        .progress
        .adjust_total(i128::from(length) - i128::from(state.size));
    state.size = length;
    state.resume_at = Some(length);
}

/// Where the copy of `file` starts: where it stopped, if `resumable` gives
/// the partial file and the source's size (the source is unchanged and the
/// target can go on writing) and the partial file still holds what was
/// confirmed; else at the start.
async fn resume_offset(
    transfer: &Transfer,
    index: usize,
    file: &PlannedFile,
    resumable: Option<(&RemotePath, u64)>,
) -> u64 {
    // Continue only a regular file: never write through a symlink planted
    // there.
    let resume_at = lock(&file.state).resume_at;
    let offset = match (resume_at, resumable) {
        (Some(at), Some((part, size))) if at > 0 => match transfer.target().lstat(part).await {
            Ok(metadata)
                if metadata.kind == EntryKind::File && metadata.size >= at && at <= size =>
            {
                at
            }
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
        state.resume_at = Some(offset);
    }
    if offset > 0 {
        transfer
            .trace
            .event("file.resume", json!({ "i": index, "offset": offset }));
    }
    offset
}

/// Writes the partial file `part` from `offset`: on the server itself when
/// the route says so and the server can, otherwise through this computer.
/// Returns its length and how it was written.
async fn write_part(
    transfer: &Transfer,
    index: usize,
    file: &PlannedFile,
    part: &RemotePath,
    offset: u64,
    size: u64,
) -> Result<(u64, &'static str)> {
    let source = transfer.source();
    let on_server = if transfer.route() == Some(Route::Server) {
        source.server_side()
    } else {
        None
    };
    let written = match on_server {
        Some(server) => {
            let progress = |confirmed: u64| {
                let mut state = lock(&file.state);
                transfer
                    .progress
                    .add_bytes(i128::from(confirmed) - i128::from(state.counted));
                state.counted = confirmed;
                state.resume_at = Some(confirmed);
                drop(state);
                transfer.journal.progressed(index, confirmed);
            };
            match server
                .copy_within(&file.source, part, offset, &progress)
                .await
            {
                Ok((length, method)) => (length, method.as_str()),
                Err(Error::Unsupported(what)) => {
                    tracing::debug!(what, "copying on the server is not possible; relaying");
                    (
                        relay(
                            transfer,
                            index,
                            file,
                            Sink::Part { path: part, offset },
                            size,
                        )
                        .await?,
                        "relay",
                    )
                }
                Err(error) => return Err(error),
            }
        }
        None => (
            relay(
                transfer,
                index,
                file,
                Sink::Part { path: part, offset },
                size,
            )
            .await?,
            "relay",
        ),
    };
    Ok(written)
}

/// Where [`relay`] writes a file.
#[derive(Clone, Copy)]
enum Sink<'a> {
    /// The partial file, from `offset`; it is renamed once complete.
    Part { path: &'a RemotePath, offset: u64 },
    /// The destination itself, on a target that writes atomically;
    /// `replace` an existing file there.
    Destination { path: &'a RemotePath, replace: bool },
}

/// Reads the source and writes it to `sink` through this computer.
/// Returns the length written. `size` is the source's size as last seen:
/// the file may grow while it is read (a log, say), but not without end.
async fn relay(
    transfer: &Transfer,
    index: usize,
    file: &PlannedFile,
    sink: Sink<'_>,
    size: u64,
) -> Result<u64> {
    let target = transfer.target();
    let offset = match sink {
        Sink::Part { offset, .. } => offset,
        Sink::Destination { .. } => 0,
    };
    let mut reader = transfer.source().read_from(&file.source, offset).await?;
    let mut writer = match sink {
        Sink::Part { path, offset } => open_partial_file(target.as_ref(), path, offset).await?,
        Sink::Destination {
            path,
            replace: true,
        } => target.open_replacing_writer(path).await?,
        Sink::Destination {
            path,
            replace: false,
        } => target.open_writer(path, 0).await?,
    };
    let most = size
        .saturating_add(size / 2)
        .saturating_add(GROWTH_ALLOWANCE);
    let mut written = offset;
    while let Some(chunk) = reader.next().await {
        let chunk = chunk?;
        let len = chunk.len() as u64;
        written = written.saturating_add(len);
        if written > most {
            // Nothing of it is worth resuming. A target that writes
            // atomically keeps nothing of an abandoned writer.
            drop(writer);
            if let Sink::Part { path, .. } = sink
                && let Err(error) = target.delete_file(path).await
            {
                tracing::debug!(%path, %error, "could not remove the partial file");
            }
            lock(&file.state).resume_at = None;
            return Err(Error::Remote {
                path: file.source.clone(),
                message: "kept growing far beyond its size while it was copied".into(),
            });
        }
        writer.write(chunk).await?;
        lock(&file.state).counted += len;
        transfer.progress.add_bytes(i128::from(len));
        if let Sink::Part { .. } = sink {
            let confirmed = writer.confirmed();
            lock(&file.state).resume_at = Some(confirmed);
            transfer.journal.progressed(index, confirmed);
        }
    }
    writer.finish().await
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
pub(super) async fn choose_destination(
    transfer: &Transfer,
    file: &PlannedFile,
    source: &Metadata,
) -> Result<Option<(RemotePath, bool)>> {
    let target = transfer.target();
    let target = target.as_ref();
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
    let same_file = transfer.same_files() && file.source == file.target;
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
