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
//! - **Routes**: the data goes through this computer, stays within one
//!   server, or flows directly between two servers (see [`route`]).
//! - **Traces**: every transfer can record what it did (see [`trace`]).
//!
//! Progress is read with [`TransferQueue::snapshot`]; state changes are
//! announced as [`TransferEvent`]s.

mod copy;
pub mod direct;
mod names;
mod plan;
pub mod route;
mod saved;
#[cfg(test)]
mod tests;
pub mod trace;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::channel::mpsc;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use self::route::{QueueOptions, Route, RoutePolicy};
use self::trace::Trace;
use crate::destination::Destination;
use crate::path::RemotePath;
use crate::remote::Remote;

pub use self::names::numbered_name;
pub use self::saved::{SavedSide, SavedTransfer, forget_saved, saved_transfers};

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
    /// The way the data takes, once decided.
    pub route: Option<Route>,
    /// Why the route is what it is, e.g. why a direct transfer was not
    /// possible.
    pub route_note: Option<String>,
    /// The trace file of this transfer, if one is recorded.
    pub trace: Option<PathBuf>,
    /// A direct job ferge started before it quit and follows again; it can
    /// be cancelled but not paused.
    pub attached: bool,
    /// The folder the items come from and the target folder, to open them
    /// (`None` if unknown).
    pub source_place: Option<Destination>,
    pub target_place: Option<Destination>,
    /// Both sides as they can be connected again (no start folder).
    pub source_server: Option<Destination>,
    pub target_server: Option<Destination>,
    /// Whether the transfer has connections to both sides; one restored
    /// after a restart has none until [`TransferQueue::reconnect`].
    pub connected: bool,
    /// Paused because a connection was lost; it goes on once both sides are
    /// connected again.
    pub lost_connection: bool,
}

/// A file that would replace an existing one.
#[derive(Clone, Debug)]
pub struct Conflict {
    pub transfer: TransferId,
    /// Whether a file or a folder is in the way.
    pub kind: ConflictKind,
    /// The existing file or folder on the target.
    pub path: RemotePath,
    pub source_size: u64,
    pub source_modified: Option<SystemTime>,
    pub existing_size: u64,
    pub existing_modified: Option<SystemTime>,
}

/// What is in the way of a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// A file exists where a file is copied to; sizes and times are those
    /// of the two files.
    File,
    /// A folder exists where a folder is copied to; sizes are 0.
    Folder,
}

/// How to resolve a [`Conflict`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    /// For a file, replace it; for a folder, merge into it (files that
    /// exist in both are asked about one by one).
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

/// Two servers could copy between themselves, bypassing this computer. To
/// do so, ferge adds a key for this transfer alone to `authorized_keys` on
/// one of them, and removes it again afterwards with everything else it
/// added; it asks before it changes anything there.
#[derive(Clone, Debug)]
pub struct DirectRequest {
    pub transfer: TransferId,
    /// The servers as shown to the user.
    pub source: String,
    pub target: String,
    /// Both servers as `user@host:port`, to remember an answer by.
    pub source_id: String,
    pub target_id: String,
}

/// The answer to a [`DirectRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectDecision {
    /// Copy directly.
    Allow,
    /// Copy through this computer.
    Relay,
}

/// Decides what happens with existing targets, and whether servers may copy
/// directly.
#[async_trait]
pub trait ConflictResolver: Send + Sync + fmt::Debug {
    /// `None` cancels the whole transfer.
    async fn resolve(&self, conflict: Conflict) -> Option<ConflictDecision>;

    /// Whether two servers may copy directly. Without an answer, the data
    /// goes through this computer.
    async fn approve_direct(&self, _request: DirectRequest) -> DirectDecision {
        DirectDecision::Relay
    }
}

/// Resolves every conflict the same way and allows direct copies, without
/// asking.
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

    async fn approve_direct(&self, _request: DirectRequest) -> DirectDecision {
        DirectDecision::Allow
    }
}

/// Announcements of a [`TransferQueue`].
#[derive(Clone, Debug)]
pub enum TransferEvent {
    /// A transfer paused because a connection was lost; reconnect it
    /// ([`TransferQueue::reconnect`]) and resume it to go on.
    ConnectionLost(TransferId),
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
    options: QueueOptions,
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
    /// A queue that copies up to `parallel_files` files at once, with the
    /// default routes and no traces. Returns the receiver of its events.
    pub fn new(parallel_files: usize) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        Self::with_options(QueueOptions::new(parallel_files))
    }

    /// A queue with `options`. Returns the receiver of its events.
    pub fn with_options(options: QueueOptions) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let (events, receiver) = mpsc::unbounded();
        let parallel_files = options.parallel_files;
        let queue = Self {
            shared: Arc::new(Shared {
                options,
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
        let trace = match &self.shared.options.trace_dir {
            Some(dir) => Trace::start(
                dir,
                id.0,
                &title,
                json!({
                    "source": request.source.label(),
                    "target": request.target.label(),
                    "target_dir": request.target_dir.as_str(),
                    "items": request.items.iter().map(RemotePath::as_str).collect::<Vec<_>>(),
                    "route_policy": format!("{:?}", self.shared.options.route),
                    "direct_options": format!("{:?}", self.shared.options.direct),
                    "source_endpoint": request.source.server_side().map(|server| format!("{:?}", server.endpoint())),
                    "target_endpoint": request.target.server_side().map(|server| format!("{:?}", server.endpoint())),
                }),
            ),
            None => Trace::disabled(),
        };
        let transfer = Arc::new(Transfer {
            id,
            title,
            trace,
            direction: Direction::between(request.source.as_ref(), request.target.as_ref()),
            servers: (
                server_of(request.source.as_ref()),
                server_of(request.target.as_ref()),
            ),
            journal: saved::Journal::default(),
            remotes: Mutex::new(Remotes {
                source: request.source,
                target: request.target,
            }),
            target_dir: request.target_dir,
            items: request.items,
            resolver,
            progress: Progress::default(),
            control: Mutex::new(Control::new()),
            conflicts: tokio::sync::Mutex::new(Remembered::default()),
            attached: Mutex::new(None),
        });
        lock(&self.shared.transfers).push(Arc::clone(&transfer));
        self.shared.emit(TransferEvent::Changed(id));
        transfer.start(&self.shared);
        id
    }

    /// Follows a direct job again that ferge started before it quit (see
    /// [`direct::recorded_jobs`]), on new connections to its two servers,
    /// until it ends, and cleans up after it. Returns `None` if this queue
    /// follows that job already. Must be called within a tokio runtime.
    pub fn attach(
        &self,
        record: direct::JobRecord,
        source: Arc<dyn Remote>,
        target: Arc<dyn Remote>,
    ) -> Option<TransferId> {
        let following = lock(&self.shared.transfers).iter().any(|transfer| {
            transfer
                .attached()
                .as_ref()
                .is_some_and(|attached| attached.job == record.job)
        });
        if following {
            return None;
        }
        let id = TransferId(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        let trace = match &self.shared.options.trace_dir {
            Some(dir) => Trace::start(
                dir,
                id.0,
                &record.title,
                json!({
                    "source": source.label(),
                    "target": target.label(),
                    "target_dir": record.target_dir.as_str(),
                    "attached": record.job,
                    "started": record.started,
                    "route_policy": format!("{:?}", self.shared.options.route),
                    "direct_options": format!("{:?}", self.shared.options.direct),
                }),
            ),
            None => Trace::disabled(),
        };
        let (files, bytes) = record.totals();
        let progress = Progress::default();
        progress.files_total.store(files, Ordering::Relaxed);
        progress.bytes_total.store(bytes, Ordering::Relaxed);
        let transfer = Arc::new(Transfer {
            id,
            title: record.title.clone(),
            trace,
            direction: Direction::between(source.as_ref(), target.as_ref()),
            servers: (server_of(source.as_ref()), server_of(target.as_ref())),
            journal: saved::Journal::default(),
            remotes: Mutex::new(Remotes { source, target }),
            target_dir: record.target_dir.clone(),
            items: record.items.clone(),
            resolver: Arc::new(FixedPolicy(ConflictAction::Skip)),
            progress,
            control: Mutex::new(Control::new()),
            conflicts: tokio::sync::Mutex::new(Remembered::default()),
            attached: Mutex::new(Some(record)),
        });
        lock(&self.shared.transfers).push(Arc::clone(&transfer));
        self.shared.emit(TransferEvent::Changed(id));
        transfer.start(&self.shared);
        Some(id)
    }

    /// All transfers, oldest first.
    pub fn snapshot(&self) -> Vec<TransferSnapshot> {
        lock(&self.shared.transfers)
            .iter()
            .map(|transfer| transfer.snapshot())
            .collect()
    }

    /// Whether a transfer that is working or paused copies from or to
    /// `remote` (this very connection, not just the same server).
    pub fn is_using(&self, remote: &Arc<dyn Remote>) -> bool {
        lock(&self.shared.transfers).iter().any(|transfer| {
            let state = transfer.state();
            (state.is_active() || state == TransferState::Paused)
                && (std::ptr::addr_eq(Arc::as_ptr(&transfer.source()), Arc::as_ptr(remote))
                    || std::ptr::addr_eq(Arc::as_ptr(&transfer.target()), Arc::as_ptr(remote)))
        })
    }

    /// Waits until no transfer [uses](Self::is_using) `remote` and no
    /// other operation holds it ([`crate::remote::InUse`]), for instance to
    /// close a connection once the work on it is done.
    pub async fn until_unused(&self, remote: &Arc<dyn Remote>) {
        while self.is_using(remote) || crate::remote::is_in_use(remote) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// Whether any transfer is working.
    pub fn is_busy(&self) -> bool {
        lock(&self.shared.transfers)
            .iter()
            .any(|transfer| transfer.state().is_active())
    }

    /// Stops a transfer, keeping partial files so it can resume.
    pub fn pause(&self, id: TransferId) {
        if let Some(transfer) = self.shared.find(id)
            && transfer.attached().is_none()
        {
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

    /// Shows a transfer saved before ferge quit (see [`saved_transfers`]),
    /// paused and not connected: [`reconnect`](Self::reconnect) and
    /// [`resume`](Self::resume) it to go on where it stopped. Must be called
    /// within a tokio runtime.
    pub fn restore(
        &self,
        saved: &SavedTransfer,
        resolver: Arc<dyn ConflictResolver>,
    ) -> TransferId {
        let id = TransferId(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        let source: Arc<dyn Remote> = Arc::new(crate::remote::offline::Offline::new(
            saved.source.label(),
            matches!(saved.source, SavedSide::Local { .. }),
        ));
        let target: Arc<dyn Remote> = Arc::new(crate::remote::offline::Offline::new(
            saved.target.label(),
            matches!(saved.target, SavedSide::Local { .. }),
        ));
        let trace = match &self.shared.options.trace_dir {
            Some(dir) => Trace::start(
                dir,
                id.0,
                &saved.title,
                json!({
                    "source": saved.source.label(),
                    "target": saved.target.label(),
                    "target_dir": saved.target_dir.as_str(),
                    "items": saved.items.iter().map(RemotePath::as_str).collect::<Vec<_>>(),
                    "restored": saved.key,
                    "saved": saved.saved,
                }),
            ),
            None => Trace::disabled(),
        };
        let plan = Arc::new(saved.plan());
        let progress = Progress::default();
        for file in &plan.files {
            let state = lock(&file.state);
            progress.files_total.fetch_add(1, Ordering::Relaxed);
            progress
                .bytes_total
                .fetch_add(state.size, Ordering::Relaxed);
            match state.status {
                plan::FileStatus::Done => {
                    progress.files_done.fetch_add(1, Ordering::Relaxed);
                    progress.bytes_done.fetch_add(state.size, Ordering::Relaxed);
                }
                plan::FileStatus::Skipped => {
                    progress.files_done.fetch_add(1, Ordering::Relaxed);
                    progress.files_skipped.fetch_add(1, Ordering::Relaxed);
                    progress.bytes_done.fetch_add(state.size, Ordering::Relaxed);
                }
                plan::FileStatus::Failed => {
                    progress.files_failed.fetch_add(1, Ordering::Relaxed);
                }
                plan::FileStatus::Pending => {}
            }
        }
        // Counted as done above; keep the files' own counters in step.
        for file in &plan.files {
            let mut state = lock(&file.state);
            if matches!(
                state.status,
                plan::FileStatus::Done | plan::FileStatus::Skipped
            ) {
                state.counted = state.size;
            }
        }
        let mut control = Control::new();
        control.state = TransferState::Paused;
        control.plan = Some(plan);
        control.route = Some(saved.route);
        control.offline = true;
        control.saved_key = Some(saved.key.clone());
        let journal = saved::Journal::default();
        if let Some(dir) = &self.shared.options.direct.records {
            journal.resume(dir, &saved.key);
        }
        let transfer = Arc::new(Transfer {
            id,
            title: saved.title.clone(),
            trace,
            direction: Direction::between(source.as_ref(), target.as_ref()),
            servers: (saved.source.destination(), saved.target.destination()),
            journal,
            remotes: Mutex::new(Remotes { source, target }),
            target_dir: saved.target_dir.clone(),
            items: saved.items.clone(),
            resolver,
            progress,
            control: Mutex::new(control),
            conflicts: tokio::sync::Mutex::new(Remembered::default()),
            attached: Mutex::new(None),
        });
        lock(&self.shared.transfers).push(transfer);
        self.shared.emit(TransferEvent::Changed(id));
        id
    }

    /// Gives a transfer that is not running new connections to its two
    /// sides: after a restart, or after a connection was lost. Returns
    /// whether it took them.
    pub fn reconnect(
        &self,
        id: TransferId,
        source: Arc<dyn Remote>,
        target: Arc<dyn Remote>,
    ) -> bool {
        let Some(transfer) = self.shared.find(id) else {
            return false;
        };
        {
            let mut control = transfer.control();
            if control.running {
                return false;
            }
            control.offline = false;
        }
        *lock(&transfer.remotes) = Remotes { source, target };
        transfer.trace.event("reconnected", json!({}));
        self.shared.emit(TransferEvent::Changed(id));
        true
    }

    /// Forgets a settled transfer. Returns whether it was removed.
    pub fn remove(&self, id: TransferId) -> bool {
        let mut transfers = lock(&self.shared.transfers);
        let before = transfers.len();
        transfers.retain(|transfer| {
            let removable = matches!(
                transfer.state(),
                TransferState::Completed | TransferState::Failed | TransferState::Cancelled
            ) || (transfer.state() == TransferState::Paused
                && transfer.control().offline);
            if transfer.id == id && removable {
                transfer.journal.forget();
            }
            transfer.id != id || !removable
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
    /// A connection was lost.
    Lost,
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
    route: Option<Route>,
    route_note: Option<String>,
    /// A direct job that runs on a server and has to be stopped or cleaned
    /// up when the transfer stops.
    direct: Option<Arc<direct::Job>>,
    /// The answer about copying directly, once asked.
    direct_decision: Option<DirectDecision>,
    /// Restored after a restart and not connected yet.
    offline: bool,
    /// Paused because a connection was lost.
    lost_connection: bool,
    /// The key under which the transfer is saved.
    saved_key: Option<String>,
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
            route: None,
            route_note: None,
            direct: None,
            direct_decision: None,
            offline: false,
            lost_connection: false,
            saved_key: None,
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

/// The two sides of a transfer; new connections replace lost ones.
#[derive(Debug)]
struct Remotes {
    source: Arc<dyn Remote>,
    target: Arc<dyn Remote>,
}

#[derive(Debug)]
struct Transfer {
    id: TransferId,
    title: String,
    trace: Trace,
    direction: Direction,
    remotes: Mutex<Remotes>,
    target_dir: RemotePath,
    items: Vec<RemotePath>,
    resolver: Arc<dyn ConflictResolver>,
    progress: Progress,
    control: Mutex<Control>,
    /// The actions chosen "for all", and a lock that keeps conflict
    /// questions of one transfer from piling up.
    conflicts: tokio::sync::Mutex<Remembered>,
    /// The direct job this transfer follows (again): after ferge restarted,
    /// or after it lost contact with the job's server.
    attached: Mutex<Option<direct::JobRecord>>,
    /// How to connect both sides again.
    servers: (Option<Destination>, Option<Destination>),
    /// Saves the transfer while it needs ferge, to resume it after a restart.
    journal: saved::Journal,
}

/// Answers that apply to all further conflicts of their kind.
#[derive(Debug, Default)]
struct Remembered {
    file: Option<ConflictAction>,
    folder: Option<ConflictAction>,
}

impl Remembered {
    fn get(&self, kind: ConflictKind) -> Option<ConflictAction> {
        match kind {
            ConflictKind::File => self.file,
            ConflictKind::Folder => self.folder,
        }
    }

    fn set(&mut self, kind: ConflictKind, action: ConflictAction) {
        match kind {
            ConflictKind::File => self.file = Some(action),
            ConflictKind::Folder => self.folder = Some(action),
        }
    }
}

impl Transfer {
    fn control(&self) -> MutexGuard<'_, Control> {
        lock(&self.control)
    }

    fn source(&self) -> Arc<dyn Remote> {
        Arc::clone(&lock(&self.remotes).source)
    }

    fn target(&self) -> Arc<dyn Remote> {
        Arc::clone(&lock(&self.remotes).target)
    }

    fn attached(&self) -> Option<direct::JobRecord> {
        lock(&self.attached).clone()
    }

    fn state(&self) -> TransferState {
        self.control().state
    }

    fn route(&self) -> Option<Route> {
        self.control().route
    }

    fn set_route(&self, route: Route, note: Option<String>, shared: &Shared) {
        tracing::info!(transfer = self.id.0, %route, note = ?note, "route chosen");
        self.trace
            .event("route", json!({ "route": route.as_str(), "note": note }));
        {
            let mut control = self.control();
            control.route = Some(route);
            control.route_note = note;
        }
        shared.emit(TransferEvent::Changed(self.id));
    }

    fn set_state(&self, state: TransferState, shared: &Shared) {
        let changed = {
            let mut control = self.control();
            let changed = control.state != state;
            control.state = state;
            changed
        };
        if changed {
            self.trace.event(
                "state",
                json!({ "state": format!("{state:?}"), "progress": self.progress_json() }),
            );
            shared.emit(TransferEvent::Changed(self.id));
        }
    }

    /// The counters as recorded in traces.
    fn progress_json(&self) -> serde_json::Value {
        let progress = &self.progress;
        json!({
            "bytes_done": progress.bytes_done.load(Ordering::Relaxed),
            "bytes_total": progress.bytes_total.load(Ordering::Relaxed),
            "files_done": progress.files_done.load(Ordering::Relaxed),
            "files_total": progress.files_total.load(Ordering::Relaxed),
            "files_skipped": progress.files_skipped.load(Ordering::Relaxed),
            "files_failed": progress.files_failed.load(Ordering::Relaxed),
        })
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
            source_label: self.source().label().to_owned(),
            target_label: self.target().label().to_owned(),
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
            route: control.route,
            route_note: control.route_note.clone(),
            trace: self.trace.path().map(std::path::Path::to_path_buf),
            attached: self.attached().is_some(),
            source_place: self
                .items
                .first()
                .and_then(RemotePath::parent)
                .and_then(|folder| {
                    self.servers
                        .0
                        .as_ref()
                        .map(|server| server.with_path(Some(folder)))
                }),
            target_place: self
                .servers
                .1
                .as_ref()
                .map(|server| server.with_path(Some(self.target_dir.clone()))),
            source_server: self.servers.0.clone(),
            target_server: self.servers.1.clone(),
            connected: !control.offline,
            lost_connection: control.lost_connection,
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
            // A cancel wins over an earlier pause request; a lost connection
            // over nothing.
            let replace = match stop {
                Stop::Lost => control.stop.is_none(),
                _ => control.stop != Some(Stop::Cancel),
            };
            if replace {
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
            if control.offline {
                // Not connected yet: see TransferQueue::reconnect.
                return;
            }
            control.errors.clear();
            control.errors_dropped = 0;
            control.lost_connection = false;
        }
        self.start(shared);
    }

    fn cancel_paused(self: &Arc<Self>, shared: &Arc<Shared>) {
        let plan = self.control().plan.clone();
        self.journal.forget();
        self.set_state(TransferState::Cancelled, shared);
        let transfer = Arc::clone(self);
        let shared = Arc::clone(shared);
        tokio::spawn(async move {
            if let Some(plan) = plan {
                plan.remove_partial_files(transfer.target().as_ref()).await;
            }
            transfer.settled(&shared);
        });
    }

    fn settled(&self, shared: &Shared) {
        shared.emit(TransferEvent::Settled {
            id: self.id,
            target: Arc::clone(&self.target()),
            target_dir: self.target_dir.clone(),
        });
    }

    async fn run(self: Arc<Self>, shared: Arc<Shared>, cancel: CancellationToken) {
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            outcome = self.work(&shared) => Some(outcome),
            () = self.sample_progress() => None,
        };
        let (stop, plan) = {
            let mut control = self.control();
            control.current = None;
            (control.stop.take(), control.plan.clone())
        };
        // A direct job keeps running on its server unless it is stopped.
        if (stop.is_some() || outcome.is_none()) && stop != Some(Stop::Lost) {
            let job = self.control().direct.take();
            if let Some(job) = job {
                job.stop().await;
            }
        }
        if let Some(Err(error)) = &outcome {
            self.trace
                .event("transfer.error", json!({ "error": error.to_string() }));
        }
        let state = match (stop, outcome) {
            (Some(Stop::Cancel), _) => {
                if let Some(plan) = &plan {
                    plan.remove_partial_files(self.target().as_ref()).await;
                }
                TransferState::Cancelled
            }
            (Some(Stop::Pause), _) | (None, None) => TransferState::Paused,
            (Some(Stop::Lost), _) => {
                self.control().lost_connection = true;
                TransferState::Paused
            }
            (None, Some(Err(error))) if error.is_connection_lost() => {
                self.control().lost_connection = true;
                TransferState::Paused
            }
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
        if matches!(state, TransferState::Completed | TransferState::Cancelled) {
            self.journal.forget();
        }
        self.set_state(state, &shared);
        self.trace
            .event("transfer.end", json!({ "state": format!("{state:?}") }));
        if self.control().lost_connection {
            tracing::info!(transfer = self.id.0, "connection lost; the transfer pauses");
            shared.emit(TransferEvent::ConnectionLost(self.id));
        }
        self.settled(&shared);
    }

    /// Records the counters once a second while the transfer runs. Never
    /// finishes.
    async fn sample_progress(&self) {
        if !self.trace.is_enabled() {
            return std::future::pending().await;
        }
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            self.trace.event("progress", self.progress_json());
        }
    }

    /// Lists what to copy and records the totals.
    async fn make_plan(&self, shared: &Shared) -> crate::Result<Arc<plan::Plan>> {
        self.set_state(TransferState::Scanning, shared);
        let started = std::time::Instant::now();
        let same_remote = self.same_files();
        let Some(targets) = plan::top_level_targets(self, same_remote).await? else {
            // A folder question was dismissed: the transfer is cancelled.
            return Ok(Arc::new(plan::Plan::default()));
        };
        let (plan, errors) = plan::Plan::scan(
            self.source().as_ref(),
            &targets,
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
        self.trace.event(
            "phase",
            json!({
                "phase": "plan",
                "ms": started.elapsed().as_secs_f64() * 1000.0,
                "files": plan.files.len(),
                "dirs": plan.dirs.len(),
                "bytes": plan.total_bytes(),
                "problems": errors.clone(),
            }),
        );
        let mut control = self.control();
        for error in errors {
            control.record_error(error);
        }
        control.plan = Some(Arc::clone(&plan));
        Ok(plan)
    }

    /// Whether a path means the same file on the source and on the target.
    fn same_files(&self) -> bool {
        if Arc::ptr_eq(&self.source(), &self.target())
            || (self.source().is_local() && self.target().is_local())
        {
            return true;
        }
        match (self.source().server_side(), self.target().server_side()) {
            (Some(source), Some(target)) => source.endpoint().same_account(target.endpoint()),
            _ => false,
        }
    }

    /// The route to try first, before anything is probed.
    fn preferred_route(&self, shared: &Shared) -> Route {
        if shared.options.route == RoutePolicy::RelayOnly {
            return Route::Relay;
        }
        let (Some(_), Some(_)) = (self.source().server_side(), self.target().server_side()) else {
            return Route::Relay;
        };
        if self.same_files() {
            Route::Server
        } else {
            Route::DirectPush
        }
    }

    /// Plans (once), creates folders and copies the remaining files.
    async fn work(self: &Arc<Self>, shared: &Arc<Shared>) -> crate::Result<()> {
        if let Some(record) = self.attached() {
            self.set_state(TransferState::Running, shared);
            shared.emit(TransferEvent::Writing {
                id: self.id,
                target: Arc::clone(&self.target()),
                target_dir: self.target_dir.clone(),
            });
            return direct::follow(self, shared, &record).await;
        }
        let planned = self.control().plan.clone();
        let plan = if let Some(plan) = planned {
            plan
        } else {
            self.make_plan(shared).await?
        };
        self.set_state(TransferState::Running, shared);
        let (problems, unusable) = plan.create_dirs(self.target().as_ref()).await;
        for error in problems {
            self.control().record_error(error);
        }
        // Nothing goes into a folder that could not be made, or that is a
        // file or a symlink: a path through a link would write wherever it
        // points.
        for file in &plan.files {
            if let Some(dir) = unusable
                .iter()
                .find(|dir| plan::is_within(&file.target, dir))
            {
                copy::fail_pending(self, file, format!("{dir} is not a folder"));
            }
        }
        shared.emit(TransferEvent::Writing {
            id: self.id,
            target: Arc::clone(&self.target()),
            target_dir: self.target_dir.clone(),
        });
        match self.preferred_route(shared) {
            Route::DirectPush | Route::DirectPull => {
                match direct::run(self, shared, &plan).await? {
                    direct::Outcome::Finished => {}
                    direct::Outcome::Unavailable(reason) => {
                        tracing::info!(transfer = self.id.0, %reason, "no direct transfer");
                        if shared.options.route == RoutePolicy::DirectOnly {
                            return Err(crate::Error::Direct(reason));
                        }
                        self.set_route(Route::Relay, Some(reason), shared);
                        self.save(shared, &plan, Route::Relay);
                        let started = std::time::Instant::now();
                        copy::copy_files(self, shared, &plan).await;
                        self.trace_phase("copy", started);
                    }
                }
            }
            route => {
                if self.route() != Some(route) {
                    self.set_route(route, None, shared);
                }
                self.save(shared, &plan, route);
                let started = std::time::Instant::now();
                copy::copy_files(self, shared, &plan).await;
                self.trace_phase("copy", started);
            }
        }
        Ok(())
    }

    /// Saves the transfer (once) so it can resume after a restart, if both
    /// sides can be connected to again.
    fn save(&self, shared: &Shared, plan: &plan::Plan, route: Route) {
        if self.journal.is_open() {
            return;
        }
        let Some(dir) = &shared.options.direct.records else {
            return;
        };
        let (source, target) = (self.source(), self.target());
        let (Some(source), Some(target)) = (
            SavedSide::of(source.as_ref()),
            SavedSide::of(target.as_ref()),
        ) else {
            return;
        };
        let key = self
            .control()
            .saved_key
            .get_or_insert_with(saved::new_key)
            .clone();
        let record = saved::record(
            &key,
            &self.title,
            (&source, &target),
            &self.target_dir,
            &self.items,
            route,
            plan,
        );
        self.journal.start(dir, &key, record);
        self.trace.event("saved", json!({ "key": key }));
    }

    fn trace_phase(&self, phase: &str, started: std::time::Instant) {
        self.trace.event(
            "phase",
            json!({ "phase": phase, "ms": started.elapsed().as_secs_f64() * 1000.0 }),
        );
    }
}

/// `remote` as a place to connect to again: this computer, or the server
/// the way it was connected (without working as root).
fn server_of(remote: &dyn Remote) -> Option<Destination> {
    remote
        .destination()
        .map(|destination| destination.with_path(None))
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
