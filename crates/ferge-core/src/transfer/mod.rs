//! Copying files and folders between remotes.
//!
//! A [`TransferQueue`] runs transfers in the background. Each transfer copies
//! one or more items (files or folders, recursively) from a source remote into
//! a directory on a target remote, which may be the same remote. Files are
//! written to `<name>.ferge-part` and renamed when complete, so a target never
//! sees half a file under its real name.
//!
//! - **Parallelism**: up to a fixed number of files are copied at once across
//!   all transfers; each file's data is pipelined by the backends.
//! - **Pause and resume**: pausing keeps the partial files; resuming continues
//!   each one at its last confirmed byte, as long as the source is unchanged.
//! - **Conflicts**: existing targets are resolved by a [`ConflictResolver`],
//!   typically a dialog, optionally once for the rest of the transfer.
//!
//! Progress is read with [`TransferQueue::snapshot`]; state changes are
//! announced as [`TransferEvent`]s.

mod copy;
mod names;
mod plan;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use async_trait::async_trait;
use futures::channel::mpsc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::path::RemotePath;
use crate::remote::Remote;

pub use self::names::numbered_name;

/// Files copied at the same time, across all transfers.
pub const DEFAULT_PARALLEL_FILES: usize = 4;

/// Error messages kept per transfer; more are only counted.
const MAX_ERRORS: usize = 100;

/// Suffix of files that are still being written.
pub const PART_SUFFIX: &str = ".ferge-part";

/// Identifies a transfer within its queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(u64);

impl TransferId {
    /// A number unique within the queue, e.g. for element ids in a UI.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Which way the data flows, for presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From this computer to a server.
    Upload,
    /// From a server to this computer.
    Download,
    /// Within one side, or between two servers.
    Copy,
}

impl Direction {
    fn between(source: &dyn Remote, target: &dyn Remote) -> Self {
        match (source.is_local(), target.is_local()) {
            (true, false) => Self::Upload,
            (false, true) => Self::Download,
            _ => Self::Copy,
        }
    }
}

/// Where a transfer stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// Started, waiting for a free slot.
    Waiting,
    /// Listing folders to find out what to copy.
    Scanning,
    Running,
    Paused,
    Completed,
    /// Finished, but some files could not be copied; see the errors.
    Failed,
    Cancelled,
}

impl TransferState {
    /// Whether work is going on or about to.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Waiting | Self::Scanning | Self::Running)
    }

    /// Whether the transfer will not continue on its own.
    pub fn is_settled(self) -> bool {
        !self.is_active()
    }
}

/// What to copy where.
#[derive(Clone, Debug)]
pub struct TransferRequest {
    pub source: Arc<dyn Remote>,
    /// Files and folders on `source`.
    pub items: Vec<RemotePath>,
    pub target: Arc<dyn Remote>,
    /// Folder on `target` that receives the items.
    pub target_dir: RemotePath,
}

/// A point-in-time view of a transfer.
#[derive(Clone, Debug)]
pub struct TransferSnapshot {
    pub id: TransferId,
    /// The item's name, or "N items".
    pub title: String,
    pub direction: Direction,
    pub source_label: String,
    pub target_label: String,
    pub target_dir: RemotePath,
    pub state: TransferState,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    /// A file being copied right now.
    pub current: Option<String>,
    pub errors: Vec<String>,
}

/// A file that would replace an existing one.
#[derive(Clone, Debug)]
pub struct Conflict {
    pub transfer: TransferId,
    /// The existing file on the target.
    pub path: RemotePath,
    pub source_size: u64,
    pub source_modified: Option<SystemTime>,
    pub existing_size: u64,
    pub existing_modified: Option<SystemTime>,
}

/// How to resolve a [`Conflict`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    Overwrite,
    Skip,
    /// Copy under a new name such as `report (2).pdf`.
    KeepBoth,
}

/// The answer to a [`Conflict`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictDecision {
    pub action: ConflictAction,
    /// Use the same action for all further conflicts of this transfer.
    pub apply_to_all: bool,
}

/// Decides what happens with existing targets.
#[async_trait]
pub trait ConflictResolver: Send + Sync + fmt::Debug {
    /// `None` cancels the whole transfer.
    async fn resolve(&self, conflict: Conflict) -> Option<ConflictDecision>;
}

/// Resolves every conflict the same way, without asking.
#[derive(Clone, Copy, Debug)]
pub struct FixedPolicy(pub ConflictAction);

#[async_trait]
impl ConflictResolver for FixedPolicy {
    async fn resolve(&self, _conflict: Conflict) -> Option<ConflictDecision> {
        Some(ConflictDecision {
            action: self.0,
            apply_to_all: true,
        })
    }
}

/// Announcements of a [`TransferQueue`].
#[derive(Clone, Debug)]
pub enum TransferEvent {
    /// A transfer was added, changed state or was removed.
    Changed(TransferId),
    /// A transfer created its folders and starts writing into `target_dir`
    /// on `target`; listings of it are out of date from now on.
    Writing {
        id: TransferId,
        target: Arc<dyn Remote>,
        target_dir: RemotePath,
    },
    /// A transfer stopped running (completed, failed, paused or cancelled);
    /// listings of `target_dir` on `target` are probably out of date.
    Settled {
        id: TransferId,
        target: Arc<dyn Remote>,
        target_dir: RemotePath,
    },
}

/// Runs transfers. Cheap to clone; all clones share the queue.
#[derive(Clone, Debug)]
pub struct TransferQueue {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    next_id: AtomicU64,
    /// One permit per file being copied.
    slots: Arc<Semaphore>,
    transfers: Mutex<Vec<Arc<Transfer>>>,
    events: mpsc::UnboundedSender<TransferEvent>,
}

impl Shared {
    fn emit(&self, event: TransferEvent) {
        let _ = self.events.unbounded_send(event);
    }

    fn find(&self, id: TransferId) -> Option<Arc<Transfer>> {
        lock(&self.transfers)
            .iter()
            .find(|transfer| transfer.id == id)
            .cloned()
    }
}

impl TransferQueue {
    /// A queue that copies up to `parallel_files` files at once. Returns the
    /// receiver of its events.
    pub fn new(parallel_files: usize) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let (events, receiver) = mpsc::unbounded();
        let queue = Self {
            shared: Arc::new(Shared {
                next_id: AtomicU64::new(1),
                slots: Arc::new(Semaphore::new(parallel_files.max(1))),
                transfers: Mutex::new(Vec::new()),
                events,
            }),
        };
        (queue, receiver)
    }

    /// Starts copying. Must be called within a tokio runtime.
    pub fn enqueue(
        &self,
        request: TransferRequest,
        resolver: Arc<dyn ConflictResolver>,
    ) -> TransferId {
        let id = TransferId(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        let title = match request.items.as_slice() {
            [single] => single.file_name().unwrap_or("/").to_owned(),
            items => format!("{} items", items.len()),
        };
        let transfer = Arc::new(Transfer {
            id,
            title,
            direction: Direction::between(request.source.as_ref(), request.target.as_ref()),
            source: request.source,
            target: request.target,
            target_dir: request.target_dir,
            items: request.items,
            resolver,
            progress: Progress::default(),
            control: Mutex::new(Control::new()),
            conflicts: tokio::sync::Mutex::new(None),
        });
        lock(&self.shared.transfers).push(Arc::clone(&transfer));
        self.shared.emit(TransferEvent::Changed(id));
        transfer.start(&self.shared);
        id
    }

    /// All transfers, oldest first.
    pub fn snapshot(&self) -> Vec<TransferSnapshot> {
        lock(&self.shared.transfers)
            .iter()
            .map(|transfer| transfer.snapshot())
            .collect()
    }

    /// Whether any transfer is working.
    pub fn is_busy(&self) -> bool {
        lock(&self.shared.transfers)
            .iter()
            .any(|transfer| transfer.state().is_active())
    }

    /// Stops a transfer, keeping partial files so it can resume.
    pub fn pause(&self, id: TransferId) {
        if let Some(transfer) = self.shared.find(id) {
            transfer.stop(Stop::Pause);
        }
    }

    /// Continues a paused transfer, or retries the failed files of one.
    /// Must be called within a tokio runtime.
    pub fn resume(&self, id: TransferId) {
        if let Some(transfer) = self.shared.find(id) {
            transfer.resume(&self.shared);
        }
    }

    /// Stops a transfer and removes its partial files.
    pub fn cancel(&self, id: TransferId) {
        if let Some(transfer) = self.shared.find(id) {
            if transfer.state() == TransferState::Paused {
                // Nothing is running that could clean up; do it here.
                transfer.cancel_paused(&self.shared);
            } else {
                transfer.stop(Stop::Cancel);
            }
        }
    }

    /// Forgets a settled transfer. Returns whether it was removed.
    pub fn remove(&self, id: TransferId) -> bool {
        let mut transfers = lock(&self.shared.transfers);
        let before = transfers.len();
        transfers.retain(|transfer| {
            transfer.id != id
                || !matches!(
                    transfer.state(),
                    TransferState::Completed | TransferState::Failed | TransferState::Cancelled
                )
        });
        let removed = transfers.len() != before;
        drop(transfers);
        if removed {
            self.shared.emit(TransferEvent::Changed(id));
        }
        removed
    }

    /// Forgets all completed and cancelled transfers.
    pub fn clear_finished(&self) {
        let removed: Vec<TransferId> = {
            let mut transfers = lock(&self.shared.transfers);
            let removed = transfers
                .iter()
                .filter(|transfer| {
                    matches!(
                        transfer.state(),
                        TransferState::Completed | TransferState::Cancelled
                    )
                })
                .map(|transfer| transfer.id)
                .collect();
            transfers.retain(|transfer| {
                !matches!(
                    transfer.state(),
                    TransferState::Completed | TransferState::Cancelled
                )
            });
            removed
        };
        for id in removed {
            self.shared.emit(TransferEvent::Changed(id));
        }
    }
}

/// Counters read by snapshots while the transfer runs.
#[derive(Debug, Default)]
struct Progress {
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    files_done: AtomicU64,
    files_total: AtomicU64,
    files_skipped: AtomicU64,
    files_failed: AtomicU64,
}

impl Progress {
    /// Moves `bytes_done` by `delta`, which may be negative when a file
    /// restarts from scratch.
    fn add_bytes(&self, delta: i128) {
        if delta >= 0 {
            let delta = u64::try_from(delta).unwrap_or(u64::MAX);
            self.bytes_done.fetch_add(delta, Ordering::Relaxed);
        } else {
            let delta = u64::try_from(-delta).unwrap_or(u64::MAX);
            let _ = self
                .bytes_done
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |done| {
                    Some(done.saturating_sub(delta))
                });
        }
    }

    fn adjust_total(&self, delta: i128) {
        let _ = self
            .bytes_total
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                Some(if delta >= 0 {
                    total.saturating_add(u64::try_from(delta).unwrap_or(u64::MAX))
                } else {
                    total.saturating_sub(u64::try_from(-delta).unwrap_or(u64::MAX))
                })
            });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Pause,
    Cancel,
}

#[derive(Debug)]
struct Control {
    state: TransferState,
    plan: Option<Arc<plan::Plan>>,
    cancel: CancellationToken,
    /// Why the running task was asked to stop.
    stop: Option<Stop>,
    running: bool,
    errors: Vec<String>,
    /// Errors beyond [`MAX_ERRORS`].
    errors_dropped: usize,
    current: Option<String>,
}

impl Control {
    fn new() -> Self {
        Self {
            state: TransferState::Waiting,
            plan: None,
            cancel: CancellationToken::new(),
            stop: None,
            running: false,
            errors: Vec::new(),
            errors_dropped: 0,
            current: None,
        }
    }

    fn record_error(&mut self, error: String) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(error);
        } else {
            self.errors_dropped += 1;
        }
    }
}

#[derive(Debug)]
struct Transfer {
    id: TransferId,
    title: String,
    direction: Direction,
    source: Arc<dyn Remote>,
    target: Arc<dyn Remote>,
    target_dir: RemotePath,
    items: Vec<RemotePath>,
    resolver: Arc<dyn ConflictResolver>,
    progress: Progress,
    control: Mutex<Control>,
    /// The action chosen "for all", and a lock that keeps conflict questions
    /// of one transfer from piling up.
    conflicts: tokio::sync::Mutex<Option<ConflictAction>>,
}

impl Transfer {
    fn control(&self) -> MutexGuard<'_, Control> {
        lock(&self.control)
    }

    fn state(&self) -> TransferState {
        self.control().state
    }

    fn set_state(&self, state: TransferState, shared: &Shared) {
        let changed = {
            let mut control = self.control();
            let changed = control.state != state;
            control.state = state;
            changed
        };
        if changed {
            shared.emit(TransferEvent::Changed(self.id));
        }
    }

    fn snapshot(&self) -> TransferSnapshot {
        let control = self.control();
        let mut errors = control.errors.clone();
        if control.errors_dropped > 0 {
            errors.push(format!("… and {} more", control.errors_dropped));
        }
        let progress = &self.progress;
        TransferSnapshot {
            id: self.id,
            title: self.title.clone(),
            direction: self.direction,
            source_label: self.source.label().to_owned(),
            target_label: self.target.label().to_owned(),
            target_dir: self.target_dir.clone(),
            state: control.state,
            bytes_done: progress.bytes_done.load(Ordering::Relaxed),
            bytes_total: progress.bytes_total.load(Ordering::Relaxed),
            files_done: progress.files_done.load(Ordering::Relaxed),
            files_total: progress.files_total.load(Ordering::Relaxed),
            files_skipped: progress.files_skipped.load(Ordering::Relaxed),
            files_failed: progress.files_failed.load(Ordering::Relaxed),
            current: control.current.clone(),
            errors,
        }
    }

    /// Spawns the task that does the work.
    fn start(self: &Arc<Self>, shared: &Arc<Shared>) {
        let cancel = {
            let mut control = self.control();
            if control.running {
                return;
            }
            control.running = true;
            control.stop = None;
            control.cancel = CancellationToken::new();
            control.state = TransferState::Waiting;
            control.cancel.clone()
        };
        shared.emit(TransferEvent::Changed(self.id));
        let transfer = Arc::clone(self);
        let shared = Arc::clone(shared);
        tokio::spawn(async move { transfer.run(shared, cancel).await });
    }

    fn stop(&self, stop: Stop) {
        let mut control = self.control();
        if control.running {
            // A cancel wins over an earlier pause request.
            if control.stop != Some(Stop::Cancel) {
                control.stop = Some(stop);
            }
            control.cancel.cancel();
        }
    }

    fn resume(self: &Arc<Self>, shared: &Arc<Shared>) {
        if !matches!(self.state(), TransferState::Paused | TransferState::Failed) {
            return;
        }
        // Retry what failed before.
        if let Some(plan) = self.control().plan.clone() {
            let retried = plan.reset_failed();
            self.progress
                .files_failed
                .fetch_sub(retried, Ordering::Relaxed);
        }
        {
            let mut control = self.control();
            control.errors.clear();
            control.errors_dropped = 0;
        }
        self.start(shared);
    }

    fn cancel_paused(self: &Arc<Self>, shared: &Arc<Shared>) {
        let plan = self.control().plan.clone();
        self.set_state(TransferState::Cancelled, shared);
        let transfer = Arc::clone(self);
        let shared = Arc::clone(shared);
        tokio::spawn(async move {
            if let Some(plan) = plan {
                plan.remove_partial_files(transfer.target.as_ref()).await;
            }
            transfer.settled(&shared);
        });
    }

    fn settled(&self, shared: &Shared) {
        shared.emit(TransferEvent::Settled {
            id: self.id,
            target: Arc::clone(&self.target),
            target_dir: self.target_dir.clone(),
        });
    }

    async fn run(self: Arc<Self>, shared: Arc<Shared>, cancel: CancellationToken) {
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            outcome = self.work(&shared) => Some(outcome),
        };
        let (stop, plan) = {
            let mut control = self.control();
            control.current = None;
            (control.stop.take(), control.plan.clone())
        };
        let state = match (stop, outcome) {
            (Some(Stop::Cancel), _) => {
                if let Some(plan) = &plan {
                    plan.remove_partial_files(self.target.as_ref()).await;
                }
                TransferState::Cancelled
            }
            (Some(Stop::Pause), _) | (None, None) => TransferState::Paused,
            (None, Some(Err(error))) => {
                self.control().record_error(error.to_string());
                TransferState::Failed
            }
            (None, Some(Ok(()))) => {
                // Problems found while scanning count as failures too.
                if self.progress.files_failed.load(Ordering::Relaxed) > 0
                    || !self.control().errors.is_empty()
                {
                    TransferState::Failed
                } else {
                    TransferState::Completed
                }
            }
        };
        self.control().running = false;
        self.set_state(state, &shared);
        self.settled(&shared);
    }

    /// Lists what to copy and records the totals.
    async fn make_plan(&self, shared: &Shared) -> crate::Result<Arc<plan::Plan>> {
        self.set_state(TransferState::Scanning, shared);
        let same_remote = Arc::ptr_eq(&self.source, &self.target)
            || (self.source.is_local() && self.target.is_local());
        let (plan, errors) = plan::Plan::scan(
            self.source.as_ref(),
            &self.items,
            &self.target_dir,
            same_remote,
        )
        .await?;
        let plan = Arc::new(plan);
        self.progress
            .bytes_total
            .store(plan.total_bytes(), Ordering::Relaxed);
        self.progress
            .files_total
            .store(plan.files.len() as u64, Ordering::Relaxed);
        let mut control = self.control();
        for error in errors {
            control.record_error(error);
        }
        control.plan = Some(Arc::clone(&plan));
        Ok(plan)
    }

    /// Plans (once), creates folders and copies the remaining files.
    async fn work(self: &Arc<Self>, shared: &Arc<Shared>) -> crate::Result<()> {
        let planned = self.control().plan.clone();
        let plan = if let Some(plan) = planned {
            plan
        } else {
            self.make_plan(shared).await?
        };
        self.set_state(TransferState::Running, shared);
        for error in plan.create_dirs(self.target.as_ref()).await {
            self.control().record_error(error);
        }
        shared.emit(TransferEvent::Writing {
            id: self.id,
            target: Arc::clone(&self.target),
            target_dir: self.target_dir.clone(),
        });
        copy::copy_files(self, shared, &plan).await;
        Ok(())
    }
}

/// `error`, prefixed with `path` unless the message names it already.
fn describe(path: &RemotePath, error: &crate::Error) -> String {
    let message = error.to_string();
    if message.contains(path.as_str()) {
        message
    } else {
        format!("{path}: {message}")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // State behind these locks stays consistent between statements; a panic
    // elsewhere must not take the whole queue down.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
