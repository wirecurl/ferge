//! Direct transfers: two servers copy between themselves over SSH while
//! ferge plans, starts and watches. What runs on the servers is described in
//! [`script`].
//!
//! Before it changes anything on the servers, ferge asks
//! ([`ConflictResolver::approve_direct`](super::ConflictResolver::approve_direct)).
//! Then, for each way the copy could go (push first, unless pull is
//! preferred):
//!
//! 1. Each side removes what earlier jobs left, notes the state of what
//!    ferge adds to, and makes its job folder; ferge uploads the scripts.
//! 2. Generate an Ed25519 key for this job alone. The private half goes to
//!    the active side (mode 0600, deleted when the job ends), the public half
//!    into the passive side's `authorized_keys`, bound to the job's forced
//!    command and an expiry time that ferge extends while it watches. The
//!    active side trusts nothing but the passive side's host key as ferge
//!    knows it.
//! 3. Probe: the active side logs in once with that key and must get the
//!    passive side's greeting.
//!
//! Anything failing on the way is recorded, cleaned up, and the next way is
//! tried; if none works, the transfer is relayed through this computer.
//!
//! With a working job, every conflict is decided up front, the file lists
//! and batches are uploaded and the driver starts detached. ferge then reads
//! its status file until it ends, removes everything the job added on both
//! sides, and checks that each server is left as it was found. The job needs
//! ferge only to start: if ferge quits or loses the connection, the copy goes
//! on and the job removes its key and folders when done.
//!
//! [`inspect`] and [`purge`] show and remove whatever ferge left on a server.

mod record;
mod script;

pub use self::record::{JobRecord, RecordedServer, forget, recorded_jobs};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt as _;
use serde_json::json;

use self::script::{Config, Entry, Mode, Role, Status};
use super::copy::{self, Outcome as FileOutcome};
use super::plan::{FileStatus, Plan, RecordedFile, part_path};
use super::route::Route;
use super::trace::{Trace, cut};
use super::{DirectDecision, DirectRequest, Shared, Transfer, lock};
use crate::entry::{EntryKind, Metadata};
use crate::error::{Error, Result};
use crate::path::{EntryName, RemotePath};
use crate::remote::Remote;
use crate::remote::server::CommandOutput;

/// How long setup commands may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// How soon ferge reads a job's status again after it changed; without news
/// the wait doubles up to the poll interval.
const FIRST_POLL: Duration = Duration::from_millis(25);

/// How often ferge checks that the driver still runs.
const ALIVE_EVERY: Duration = Duration::from_secs(5);

/// Uploads and conflict checks done at the same time.
const AT_ONCE: usize = 16;

/// Bytes of a server-side file kept in the trace.
const ARTIFACT_LIMIT: usize = 1024 * 1024;

/// The slowest rate assumed between two servers, for how long a job's key
/// lasts before ferge renews it.
const ASSUMED_RATE: u64 = 1_000_000;

/// The longest a job's key lasts without being renewed.
const KEY_LIFETIME_MAX: Duration = Duration::from_hours(72);

/// What ferge adds to on a server, as the scripts name it and as shown.
const PLACES: &[(&str, &str)] = &[
    ("home", "~"),
    ("cache", "~/.cache"),
    ("ferge", "~/.cache/ferge"),
    ("jobs", "~/.cache/ferge/jobs"),
    ("ssh", "~/.ssh"),
    ("keys", "~/.ssh/authorized_keys"),
];

/// How a direct attempt ended.
pub(super) enum Outcome {
    /// Every file was handled (copied, skipped or failed).
    Finished,
    /// No direct way works, or it was declined; why.
    Unavailable(String),
}

/// Copies the pending files of `plan` directly between the two servers, or
/// reports why that is not possible.
pub(super) async fn run(
    transfer: &Arc<Transfer>,
    shared: &Arc<Shared>,
    plan: &Arc<Plan>,
) -> Result<Outcome> {
    let pending: Vec<usize> = plan
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| lock(&file.state).status == FileStatus::Pending)
        .map(|(index, _)| index)
        .collect();
    if pending.is_empty() {
        return Ok(Outcome::Finished);
    }
    if let Err(reason) = approve(transfer, shared).await {
        return Ok(Outcome::Unavailable(reason));
    }
    let bytes: u64 = pending
        .iter()
        .map(|index| lock(&plan.files[*index].state).size)
        .sum();

    let started = Instant::now();
    let (ready, reasons) = find_way(transfer, shared, bytes).await;
    transfer.trace_phase("setup", started);
    let Some((mode, job)) = ready else {
        return Ok(Outcome::Unavailable(reasons.join("; ")));
    };
    transfer.control().direct = Some(Arc::clone(&job));
    let route = match mode {
        Mode::Push => Route::DirectPush,
        Mode::Pull => Route::DirectPull,
    };
    transfer.set_route(
        route,
        (!reasons.is_empty()).then(|| reasons.join("; ")),
        shared,
    );

    // The job runs without asking, so every conflict is decided now.
    let started = Instant::now();
    let entries = entries(transfer, plan, &pending, mode).await;
    transfer.trace_phase("destinations", started);
    if entries.is_empty() {
        job.finish().await;
        transfer.control().direct = None;
        return Ok(Outcome::Finished);
    }

    let started = Instant::now();
    let launched = async {
        job.upload_files(&entries, &transfer.target_dir).await?;
        job.save_record(
            transfer,
            plan,
            &entries,
            shared.options.direct.records.as_deref(),
        )
        .await;
        job.start().await
    }
    .await;
    transfer.trace_phase("launch", started);
    if let Err(reason) = launched {
        job.finish().await;
        transfer.control().direct = None;
        return Ok(Outcome::Unavailable(reason));
    }

    let started = Instant::now();
    let watched = job.watch(transfer, plan).await;
    transfer.trace_phase("copy", started);
    let code = match watched {
        Ok(code) => code,
        Err(error) => {
            // The job goes on without ferge; it is not cleaned up here. To
            // go on, the transfer follows it again.
            let record = lock(&job.state).job_record.clone();
            *lock(&transfer.attached) = record;
            if error.is_connection_lost() {
                return Err(error);
            }
            return Err(Error::Direct(format!(
                "lost contact with {} ({error}); job {} goes on there and removes its key when done",
                job.active.remote.label(),
                job.config.job,
            )));
        }
    };
    let started = Instant::now();
    job.finish().await;
    transfer.control().direct = None;
    transfer.trace_phase("cleanup", started);

    fail_leftovers(transfer, plan, &entries, code);
    Ok(Outcome::Finished)
}

/// Follows a job ferge started before it quit (see [`JobRecord`]) until it
/// ends, then cleans up after it like after any job.
pub(super) async fn follow(
    transfer: &Arc<Transfer>,
    shared: &Arc<Shared>,
    record: &JobRecord,
) -> Result<()> {
    // After a lost connection the plan is still there, with what was done
    // since; after a restart it comes from the record.
    let existing = transfer.control().plan.clone();
    let plan = if let Some(plan) = existing {
        plan
    } else {
        let plan = Arc::new(Plan::recorded(&record.files));
        transfer.control().plan = Some(Arc::clone(&plan));
        plan
    };
    let job = Job::reattach(record, transfer, shared).await?;
    transfer.control().direct = Some(Arc::clone(&job));
    let route = match record.mode {
        Mode::Push => Route::DirectPush,
        Mode::Pull => Route::DirectPull,
    };
    transfer.set_route(route, None, shared);
    let (files, bytes) = record.totals();
    transfer.trace.event(
        "direct.attach",
        json!({
            "job": record.job,
            "files": files,
            "bytes": bytes,
            "started": record.started,
        }),
    );
    tracing::info!(job = %record.job, "following a direct transfer again");

    // A job that ended long ago may be gone: tidied by a later job.
    let status = job.active.path("status")?;
    let code = if matches!(
        job.active.remote.stat(&status).await,
        Err(Error::NotFound { .. })
    ) {
        VANISHED
    } else {
        let started = Instant::now();
        let watched = job.watch(transfer, &plan).await;
        transfer.trace_phase("copy", started);
        match watched {
            Ok(code) => code,
            Err(error) if error.is_connection_lost() => return Err(error),
            Err(error) => {
                return Err(Error::Direct(format!(
                    "lost contact with {} again ({error}); job {} goes on there",
                    job.active.remote.label(),
                    record.job,
                )));
            }
        }
    };
    let started = Instant::now();
    job.finish().await;
    transfer.control().direct = None;
    transfer.trace_phase("cleanup", started);
    let indexes: Vec<usize> = record.files.iter().map(|file| file.index).collect();
    fail_unreported(transfer, &plan, &indexes, code);
    Ok(())
}

/// The active and passive side of `mode`.
fn sides_of(transfer: &Transfer, mode: Mode) -> (Arc<dyn Remote>, Arc<dyn Remote>) {
    match mode {
        Mode::Push => (transfer.source(), transfer.target()),
        Mode::Pull => (transfer.target(), transfer.source()),
    }
}

/// Asks once per transfer whether the servers may copy directly, if they
/// could at all; `Err` says why not.
async fn approve(transfer: &Transfer, shared: &Shared) -> std::result::Result<(), String> {
    let options = &shared.options.direct;
    let mut reasons = Vec::new();
    for mode in [Mode::Push, Mode::Pull] {
        let (active, passive) = sides_of(transfer, mode);
        if let Err(reason) = peer(active.as_ref(), passive.as_ref(), options) {
            reasons.push(format!("{}: {reason}", mode.as_str()));
        }
    }
    if reasons.len() == 2 {
        return Err(reasons.join("; "));
    }
    let asked = transfer.control().direct_decision;
    let decision = if let Some(decision) = asked {
        decision
    } else {
        let request = DirectRequest {
            transfer: transfer.id,
            source: transfer.source().label().to_owned(),
            target: transfer.target().label().to_owned(),
            source_id: identity(transfer.source().as_ref()),
            target_id: identity(transfer.target().as_ref()),
        };
        let started = Instant::now();
        let decision = transfer.resolver.approve_direct(request).await;
        transfer.control().direct_decision = Some(decision);
        transfer.trace.event(
            "direct.approval",
            json!({
                "allowed": decision == DirectDecision::Allow,
                "ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        );
        decision
    };
    match decision {
        DirectDecision::Allow => Ok(()),
        DirectDecision::Relay => Err("you chose to copy through this computer".to_owned()),
    }
}

/// A server as `user@host:port`.
fn identity(remote: &dyn Remote) -> String {
    remote.server_side().map_or_else(
        || remote.label().to_owned(),
        |server| {
            let endpoint = server.endpoint();
            format!("{}@{}", endpoint.user, endpoint.address())
        },
    )
}

/// Tries each way in turn; returns the first that works and why the others
/// did not.
async fn find_way(
    transfer: &Transfer,
    shared: &Shared,
    bytes: u64,
) -> (Option<(Mode, Arc<Job>)>, Vec<String>) {
    let modes = if shared.options.direct.prefer_pull {
        [Mode::Pull, Mode::Push]
    } else {
        [Mode::Push, Mode::Pull]
    };
    let mut reasons = Vec::new();
    for mode in modes {
        match prepare(transfer, shared, mode, bytes).await {
            Ok(job) => return (Some((mode, job)), reasons),
            Err(reason) => {
                tracing::info!(mode = mode.as_str(), %reason, "direct transfer not possible");
                transfer.trace.event(
                    "direct.unavailable",
                    json!({ "mode": mode.as_str(), "reason": reason }),
                );
                reasons.push(format!("{}: {reason}", mode.as_str()));
            }
        }
    }
    (None, reasons)
}

/// Fails the files the job never got to, saying why from its exit `code`.
fn fail_leftovers(transfer: &Transfer, plan: &Plan, entries: &[Entry], code: i32) {
    let indexes: Vec<usize> = entries.iter().map(|entry| entry.index).collect();
    fail_unreported(transfer, plan, &indexes, code);
}

/// Code for a job whose folder is gone from its server.
const VANISHED: i32 = -2;

fn fail_unreported(transfer: &Transfer, plan: &Plan, indexes: &[usize], code: i32) {
    let message = match code {
        0 => "the job ended without copying this file".to_owned(),
        VANISHED => "the job ended while ferge was away and its status is gone; \
                     check this file on the target"
            .to_owned(),
        70 => "the servers could not connect to each other".to_owned(),
        143 => "the job was stopped".to_owned(),
        -1 => "the job ended unexpectedly".to_owned(),
        code => format!("the job ended with code {code}"),
    };
    for index in indexes {
        let Some(file) = plan.files.get(*index) else {
            continue;
        };
        if lock(&file.state).status == FileStatus::Pending {
            copy::finish_file(
                transfer,
                file,
                Err(Error::Remote {
                    path: file.source.clone(),
                    message: message.clone(),
                }),
            );
        }
    }
}

/// Sets up a job for `mode`, ending with a successful probe; or why not.
async fn prepare(
    transfer: &Transfer,
    shared: &Shared,
    mode: Mode,
    bytes: u64,
) -> std::result::Result<Arc<Job>, String> {
    let options = &shared.options.direct;
    let (active, passive) = sides_of(transfer, mode);
    let peer = peer(active.as_ref(), passive.as_ref(), options)?;
    let config = Config {
        job: job_id().map_err(|error| error.to_string())?,
        mode,
        peer_host: peer.host,
        peer_port: peer.port,
        peer_user: peer.user,
        workers: options.workers,
        keep: options.keep_jobs,
        verbose: options.ssh_verbose,
        linger: options.linger.as_secs().max(1),
    };
    let dialed = format!("{}:{}", config.peer_host, config.peer_port);
    let dir = |home: RemotePath| {
        RemotePath::parse(&format!(
            "{}/{}",
            home.as_str().trim_end_matches('/'),
            config.dir()
        ))
    };
    let home = |remote: &Arc<dyn Remote>| {
        let remote = Arc::clone(remote);
        async move { remote.home_dir().await }
    };
    let active_home = home(&active)
        .await
        .map_err(|error| format!("no login directory: {error}"))?;
    let passive_home = home(&passive)
        .await
        .map_err(|error| format!("no login directory: {error}"))?;
    let lifetime = key_lifetime(options.key_lifetime, bytes);
    let job = Arc::new(Job {
        active: Side {
            name: "active",
            role: Role::Active,
            remote: Arc::clone(&active),
            dir: dir(active_home).map_err(|error| error.to_string())?,
        },
        passive: Side {
            name: "passive",
            role: Role::Passive,
            remote: Arc::clone(&passive),
            dir: dir(passive_home).map_err(|error| error.to_string())?,
        },
        trace: transfer.trace.clone(),
        poll_interval: options.poll_interval,
        batching: options.batching,
        lifetime,
        coarse_clock: std::sync::atomic::AtomicBool::new(false),
        finished: tokio::sync::Mutex::new(false),
        state: std::sync::Mutex::new(JobState::default()),
        config,
    });
    transfer.trace.event(
        "direct.attempt",
        json!({
            "mode": mode.as_str(),
            "job": job.config.job,
            "active": active.label(),
            "passive": passive.label(),
            "dial": dialed,
            "workers": job.config.workers,
            "key_lifetime_s": lifetime.as_secs(),
        }),
    );
    match job.setup(&peer.host_key).await {
        Ok(()) => Ok(job),
        Err(reason) => {
            job.finish().await;
            Err(reason)
        }
    }
}

/// How the active side reaches the passive one.
struct Peer {
    host: String,
    port: u16,
    user: String,
    host_key: String,
}

/// Checks that `active` can dial `passive` and work there; or why not.
fn peer(
    active: &dyn Remote,
    passive: &dyn Remote,
    options: &super::route::DirectOptions,
) -> std::result::Result<Peer, String> {
    let (Some(active_server), Some(passive_server)) = (active.server_side(), passive.server_side())
    else {
        return Err("both sides must be SSH servers".to_owned());
    };
    for (remote, server) in [(active, active_server), (passive, passive_server)] {
        if !server.runs_commands() {
            return Err(format!(
                "{} does not run commands in this session (root sessions cannot)",
                remote.label()
            ));
        }
    }
    let endpoint = passive_server.endpoint();
    if endpoint.host_key.is_empty() {
        return Err(format!("the host key of {} is unknown", passive.label()));
    }
    let address = endpoint.address();
    if endpoint.proxied && !options.has_override(&address) {
        return Err(format!(
            "{} is reached through a proxy, so {} cannot dial it (set FERGE_DIRECT_ADDRESS)",
            passive.label(),
            active.label()
        ));
    }
    let dialed = options.address_for(&address);
    let (host, port) = split_address(&dialed).ok_or_else(|| format!("bad address {dialed}"))?;
    if !script::is_plain(&host) || !script::is_plain(&endpoint.user) {
        return Err(format!(
            "the host or user name of {} needs quoting",
            passive.label()
        ));
    }
    Ok(Peer {
        host,
        port,
        user: endpoint.user.clone(),
        host_key: endpoint.host_key.clone(),
    })
}

/// Decides where each pending file goes and where it resumes.
async fn entries(transfer: &Transfer, plan: &Plan, pending: &[usize], mode: Mode) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(pending.len());
    for group in pending.chunks(AT_ONCE) {
        let mut checks = Vec::with_capacity(group.len());
        for index in group {
            checks.push(entry(transfer, plan, *index, mode));
        }
        entries.extend(
            futures::future::join_all(checks)
                .await
                .into_iter()
                .flatten(),
        );
    }
    entries
}

async fn entry(transfer: &Transfer, plan: &Plan, index: usize, mode: Mode) -> Option<Entry> {
    let file = &plan.files[index];
    let (size, modified) = refresh(transfer, file).await?;
    // Decided in an earlier attempt, which may have got this file in place.
    let decided = lock(&file.state).destination.is_some();
    let (destination, replace) = destination(transfer, file, index, size, modified).await?;
    let part = part_path(&destination)?;
    let resume_at = lock(&file.state).resume_at;
    // A job that was stopped may have finished a file without reporting it:
    // it is in place, with the planned size and time.
    if decided
        && let Ok(existing) = transfer.target().stat(&destination).await
        && existing.size == size
        && existing.modified.is_some()
        && existing.modified == modified.map(whole_seconds)
        && transfer.target().stat(&part).await.is_err()
    {
        {
            let mut state = lock(&file.state);
            transfer
                .progress
                .add_bytes(i128::from(state.size) - i128::from(state.counted));
            state.counted = state.size;
        }
        transfer.trace.event(
            "file.done",
            json!({ "i": index, "bytes": size, "method": "direct-earlier" }),
        );
        copy::finish_file(transfer, file, Ok(FileOutcome::Copied("direct")));
        return None;
    }
    let offset = match resume_at {
        Some(at) if at > 0 && at <= size => match transfer.target().stat(&part).await {
            Ok(metadata) if metadata.size >= at => at,
            _ => 0,
        },
        _ => 0,
    };
    let (active_path, passive_path) = match mode {
        Mode::Push => (file.source.as_str(), destination.as_str()),
        Mode::Pull => (destination.as_str(), file.source.as_str()),
    };
    Some(Entry {
        index,
        offset,
        size,
        mtime: modified.and_then(touch_time),
        replace,
        active_path: active_path.to_owned(),
        passive_path: passive_path.to_owned(),
    })
}

/// The file's size and time; a file tried before may have changed since it
/// was planned. `None` if it cannot be read any more (the file failed).
async fn refresh(
    transfer: &Transfer,
    file: &super::plan::PlannedFile,
) -> Option<(u64, Option<std::time::SystemTime>)> {
    let (size, modified, tried) = {
        let state = lock(&file.state);
        (state.size, state.modified, state.resume_at.is_some())
    };
    if !tried {
        return Some((size, modified));
    }
    match transfer.source().stat(&file.source).await {
        Ok(current) => {
            let mut state = lock(&file.state);
            transfer
                .progress
                .adjust_total(i128::from(current.size) - i128::from(state.size));
            if (state.size, state.modified) != (current.size, current.modified) {
                state.resume_at = None;
            }
            state.size = current.size;
            state.modified = current.modified;
            Some((current.size, current.modified))
        }
        Err(error) => {
            copy::finish_file(transfer, file, Err(error));
            None
        }
    }
}

/// Where the file goes and whether it replaces one, asking about conflicts
/// if that is not decided yet. `None` if it is skipped or failed.
async fn destination(
    transfer: &Transfer,
    file: &super::plan::PlannedFile,
    index: usize,
    size: u64,
    modified: Option<std::time::SystemTime>,
) -> Option<(RemotePath, bool)> {
    let decided = lock(&file.state).destination.clone();
    if let Some(destination) = decided {
        return Some(destination);
    }
    let metadata = Metadata {
        kind: EntryKind::File,
        size,
        modified,
        permissions: None,
    };
    match copy::choose_destination(transfer, file, &metadata).await {
        Ok(Some(destination)) => {
            lock(&file.state).destination = Some(destination.clone());
            Some(destination)
        }
        Ok(None) => {
            transfer.trace.event("file.skip", json!({ "i": index }));
            copy::finish_file(transfer, file, Ok(FileOutcome::Skipped));
            None
        }
        Err(error) => {
            copy::finish_file(transfer, file, Err(error));
            None
        }
    }
}

/// `time` without its fraction of a second, as `touch -t` sets it.
fn whole_seconds(time: std::time::SystemTime) -> std::time::SystemTime {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => std::time::UNIX_EPOCH + Duration::from_secs(elapsed.as_secs()),
        Err(_) => time,
    }
}

/// `time` in UTC as `touch -t` takes it.
fn touch_time(time: std::time::SystemTime) -> Option<String> {
    let timestamp = jiff::Timestamp::try_from(time).ok()?;
    Some(timestamp.strftime("%Y%m%d%H%M.%S").to_string())
}

/// `host:port` or `[v6]:port`.
fn split_address(address: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        (host, port)
    } else {
        address.rsplit_once(':')?
    };
    Some((host.to_owned(), port.parse().ok()?))
}

fn job_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| Error::protocol("no randomness", error))?;
    Ok(bytes.iter().fold(String::from("j"), |mut id, byte| {
        let _ = write!(id, "{byte:02x}");
        id
    }))
}

/// A fresh Ed25519 key: the private half in OpenSSH format, the public half
/// as an `authorized_keys` key.
fn generate_key(job: &str) -> Result<(zeroize::Zeroizing<String>, String)> {
    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::keys::ssh_key::{LineEnding, PrivateKey};

    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::fill(seed.as_mut()).map_err(|error| Error::protocol("no randomness", error))?;
    let mut key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    key.set_comment(format!("ferge-job-{job}"));
    let private = key
        .to_openssh(LineEnding::LF)
        .map_err(|error| Error::protocol("encoding a key failed", error))?;
    let public = key
        .public_key()
        .to_openssh()
        .map_err(|error| Error::protocol("encoding a key failed", error))?;
    Ok((private, public))
}

/// How long a job's key lasts before ferge renews it: at least `least`,
/// longer for many bytes, at most three days.
fn key_lifetime(least: Duration, bytes: u64) -> Duration {
    (least + Duration::from_secs(bytes / ASSUMED_RATE)).min(KEY_LIFETIME_MAX)
}

/// The `expiry-time` for a key that lasts `lifetime` from now, in the
/// server's time (sshd reads it so). `zone` is the server's offset from UTC
/// in seconds; without it, the margin covers every time zone.
fn expiry(lifetime: Duration, zone: Option<i32>) -> String {
    let margin = if zone.is_some() {
        Duration::from_mins(15)
    } else {
        Duration::from_hours(14)
    };
    let now = jiff::Timestamp::now();
    let at = jiff::SignedDuration::try_from(lifetime + margin)
        .ok()
        .and_then(|span| now.checked_add(span).ok())
        .unwrap_or(now);
    let offset = zone
        .and_then(|seconds| jiff::tz::Offset::from_seconds(seconds).ok())
        .unwrap_or(jiff::tz::Offset::UTC);
    offset.to_datetime(at).strftime("%Y%m%d%H%M").to_string()
}

/// `date +%z` (`+0100`) as seconds east of UTC.
fn parse_zone(zone: &str) -> Option<i32> {
    let zone = zone.trim();
    let (sign, digits) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// The state of what ferge adds to on one server, from `note` and `entry`
/// lines.
#[derive(Clone, Debug, Default)]
struct Footprint {
    /// Name → `-` (absent) or `<time> <checksum>`.
    notes: BTreeMap<String, String>,
    /// Name → the entries of a folder.
    entries: BTreeMap<String, BTreeSet<String>>,
}

impl Footprint {
    fn parse(text: &str) -> Self {
        let mut footprint = Self::default();
        footprint.absorb(text);
        footprint
    }

    /// The `note` and `entry` lines this was read from.
    fn to_text(&self) -> String {
        let mut text = String::new();
        for (name, value) in &self.notes {
            let _ = writeln!(text, "note {name} {value}");
            for entry in self.entries.get(name).into_iter().flatten() {
                let _ = writeln!(text, "entry {name} {entry}");
            }
        }
        text
    }

    /// Takes over the `note` and `entry` lines of `text`; a later note of a
    /// name replaces an earlier one.
    fn absorb(&mut self, text: &str) {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("note ") {
                let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
                self.notes.insert(name.to_owned(), value.to_owned());
                self.entries.remove(name);
            } else if let Some(rest) = line.strip_prefix("entry ") {
                let (name, entry) = rest.split_once(' ').unwrap_or((rest, ""));
                if !matches!(entry, "." | "..") {
                    self.entries
                        .entry(name.to_owned())
                        .or_default()
                        .insert(entry.to_owned());
                }
            }
        }
    }

    /// How `after` differs from this, in words.
    fn differences(&self, after: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        for (name, label) in PLACES {
            let (Some(before), Some(now)) = (self.notes.get(*name), after.notes.get(*name)) else {
                continue;
            };
            if before == now {
                continue;
            }
            if before == "-" {
                differences.push(format!("{label} was added"));
                continue;
            }
            if now == "-" {
                differences.push(format!("{label} was removed"));
                continue;
            }
            let (time_before, state_before) = before.split_once(' ').unwrap_or((before, ""));
            let (time_now, state_now) = now.split_once(' ').unwrap_or((now, ""));
            if state_before != state_now {
                let empty = BTreeSet::new();
                let old = self.entries.get(*name).unwrap_or(&empty);
                let new = after.entries.get(*name).unwrap_or(&empty);
                let added: Vec<&str> = new.difference(old).map(String::as_str).collect();
                let removed: Vec<&str> = old.difference(new).map(String::as_str).collect();
                if added.is_empty() && removed.is_empty() {
                    differences.push(format!("{label}: contents changed"));
                } else {
                    let mut text = format!("{label}:");
                    if !added.is_empty() {
                        let _ = write!(text, " added {}", added.join(", "));
                    }
                    if !removed.is_empty() {
                        let _ = write!(text, " removed {}", removed.join(", "));
                    }
                    differences.push(text);
                }
            } else if time_before != time_now {
                differences.push(format!("{label}: modification time changed"));
            }
        }
        differences
    }
}

/// One server of a job.
#[derive(Debug)]
struct Side {
    name: &'static str,
    role: Role,
    remote: Arc<dyn Remote>,
    /// The job folder.
    dir: RemotePath,
}

impl Side {
    fn path(&self, name: &str) -> Result<RemotePath> {
        let mut path = self.dir.clone();
        for part in name.split('/') {
            path = path.join(&EntryName::new(part)?);
        }
        Ok(path)
    }
}

/// What ferge learns about a job as it goes.
#[derive(Debug, Default)]
struct JobState {
    /// Each side's state before the job, by side name.
    before: HashMap<&'static str, Footprint>,
    /// The passive side's offset from UTC, in seconds.
    zone: Option<i32>,
    /// Whether each side has `tar` (needed for batches).
    active_tar: bool,
    passive_tar: bool,
    /// The staging folder on the target side, if the job has batches.
    stage: Option<String>,
    /// Where each file of the job starts.
    offsets: HashMap<usize, u64>,
    /// The files of each batch, and the batch of each batched file.
    batches: Vec<Vec<usize>>,
    batch_of: HashMap<usize, usize>,
    /// When ferge renews the job's key next.
    renew_at: Option<Instant>,
    /// The record of the job on this computer, once written.
    record: Option<std::path::PathBuf>,
    /// What the record holds, to follow the job again after losing contact.
    job_record: Option<JobRecord>,
}

/// Files and batches in flight, with their start times on the server.
#[derive(Debug, Default)]
struct InFlight {
    files: HashMap<usize, f64>,
    batches: HashMap<usize, f64>,
}

/// A direct transfer job on two servers.
#[derive(Debug)]
pub(super) struct Job {
    config: Config,
    active: Side,
    passive: Side,
    trace: Trace,
    poll_interval: Duration,
    /// Whether small files may go in batches.
    batching: bool,
    /// How long the job's key lasts before it is renewed.
    lifetime: Duration,
    /// The driver's clock counts whole seconds only: no durations per file.
    coarse_clock: std::sync::atomic::AtomicBool,
    /// Set once cleaned up.
    finished: tokio::sync::Mutex<bool>,
    state: std::sync::Mutex<JobState>,
}

impl Job {
    fn side(&self, role: Role) -> &Side {
        match role {
            Role::Active => &self.active,
            Role::Passive => &self.passive,
        }
    }

    /// Runs `command` on `side` (with `input` on standard input) and
    /// records it in the trace; `script` names a script sent as input.
    async fn exec(
        &self,
        side: &Side,
        command: &str,
        input: Option<&[u8]>,
        script: Option<&str>,
    ) -> Result<CommandOutput> {
        let server = side
            .remote
            .server_side()
            .ok_or(Error::Unsupported("running commands"))?;
        let started = Instant::now();
        let result = server.run(command, input, COMMAND_TIMEOUT).await;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut event = json!({
            "side": side.name,
            "remote": side.remote.label(),
            "command": command,
            "ms": ms,
        });
        if let Some(script) = script {
            event["script"] = json!(script);
        }
        match &result {
            Ok(output) => {
                event["status"] = json!(output.status);
                event["stdout"] = json!(cut(&output.stdout_text()));
                event["stderr"] = json!(cut(&output.stderr_text()));
            }
            Err(error) => event["error"] = json!(error.to_string()),
        }
        self.trace.event("exec", event);
        result
    }

    async fn run(&self, side: &Side, command: &str, input: Option<&[u8]>) -> Result<CommandOutput> {
        self.exec(side, command, input, None).await
    }

    /// Runs a script with `sh -s`.
    async fn script(&self, side: &Side, name: &str, text: &str) -> Result<CommandOutput> {
        self.exec(side, "sh -s", Some(text.as_bytes()), Some(name))
            .await
    }

    /// Runs `command` and requires it to succeed; the error says what it
    /// printed.
    async fn require(&self, side: &Side, command: &str) -> std::result::Result<String, String> {
        let output = self
            .run(side, command, None)
            .await
            .map_err(|error| format!("{}: {error}", side.remote.label()))?;
        Self::succeeded(side, command, &output)
    }

    /// Runs a script and requires it to succeed.
    async fn require_script(
        &self,
        side: &Side,
        name: &str,
        text: &str,
    ) -> std::result::Result<String, String> {
        let output = self
            .script(side, name, text)
            .await
            .map_err(|error| format!("{}: {error}", side.remote.label()))?;
        Self::succeeded(side, name, &output)
    }

    fn succeeded(
        side: &Side,
        what: &str,
        output: &CommandOutput,
    ) -> std::result::Result<String, String> {
        if output.success() {
            return Ok(output.stdout_text().into_owned());
        }
        let stderr = output.stderr_text();
        Err(format!(
            "{}: `{what}` failed ({}): {}",
            side.remote.label(),
            output
                .status
                .map_or_else(|| "no status".to_owned(), |status| status.to_string()),
            stderr.trim()
        ))
    }

    /// Writes `contents` to `name` in the side's job folder.
    async fn upload(&self, side: &Side, name: &str, contents: Bytes, mode: u32) -> Result<()> {
        let path = side.path(name)?;
        let len = contents.len();
        let started = Instant::now();
        let stream = futures::stream::once(std::future::ready(Ok(contents))).boxed();
        side.remote.write(&path, stream).await?;
        side.remote.chmod(&path, mode).await?;
        self.trace.event(
            "upload",
            json!({
                "side": side.name,
                "name": name,
                "bytes": len,
                "ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        );
        Ok(())
    }

    /// Uploads files, a few at a time.
    async fn upload_all(
        &self,
        uploads: Vec<(&Side, String, Bytes, u32)>,
    ) -> std::result::Result<(), String> {
        let mut uploads = uploads.into_iter().peekable();
        while uploads.peek().is_some() {
            let mut group = Vec::with_capacity(AT_ONCE);
            for (side, name, contents, mode) in uploads.by_ref().take(AT_ONCE) {
                group.push(async move {
                    self.upload(side, &name, contents, mode)
                        .await
                        .map_err(|error| {
                            format!("{}: uploading {name}: {error}", side.remote.label())
                        })
                });
            }
            futures::future::try_join_all(group).await?;
        }
        Ok(())
    }

    /// Prepares both job folders, installs the job's key and probes.
    async fn setup(&self, host_key: &str) -> std::result::Result<(), String> {
        let config = &self.config;
        let text = script::setup_script(config);
        let (active, passive) = tokio::join!(
            self.require_script(&self.active, "setup", &text),
            self.require_script(&self.passive, "setup", &text),
        );
        for (side, output) in [(&self.active, active?), (&self.passive, passive?)] {
            if !output.lines().any(|line| line == "ready") {
                return Err(format!(
                    "{}: the job folder could not be made",
                    side.remote.label()
                ));
            }
            self.absorb_setup(side, &output);
        }

        let (private, public) = generate_key(&config.job).map_err(|error| error.to_string())?;
        let zone = lock(&self.state).zone;
        let until = expiry(self.lifetime, zone);
        let mut uploads = vec![(
            &self.active,
            "key".to_owned(),
            Bytes::from(private.as_bytes().to_vec()),
            0o600,
        )];
        for (active, name, text, mode) in scripts(config, host_key, &public, &until) {
            let side = if active { &self.active } else { &self.passive };
            uploads.push((side, name.to_owned(), Bytes::from(text), mode));
        }
        self.upload_all(uploads).await?;

        let installed = self
            .require(&self.passive, &format!("sh {}/install.sh", config.dir()))
            .await?;
        if !installed.lines().any(|line| line == "installed") {
            return Err(format!(
                "{}: the job key could not be installed",
                self.passive.remote.label()
            ));
        }
        {
            let mut state = lock(&self.state);
            state
                .before
                .entry(self.passive.name)
                .or_default()
                .absorb(&installed);
            state.renew_at = Some(Instant::now() + self.lifetime / 2);
        }
        self.trace.event(
            "direct.key",
            json!({ "expiry": until, "zone_s": zone, "lifetime_s": self.lifetime.as_secs() }),
        );
        self.probe().await
    }

    /// Records what a side's setup printed: its state, its time zone, and
    /// what it swept up after earlier jobs.
    fn absorb_setup(&self, side: &Side, output: &str) {
        let mut swept = Vec::new();
        let mut results: BTreeMap<String, String> = BTreeMap::new();
        let mut result: Option<String> = None;
        for line in output.lines() {
            if let Some(text) = line.strip_prefix("| ") {
                if let Some(job) = &result {
                    let text_so_far = results.entry(job.clone()).or_default();
                    text_so_far.push_str(text);
                    text_so_far.push('\n');
                }
                continue;
            }
            result = None;
            if let Some(job) = line.strip_prefix("result ") {
                result = Some(job.to_owned());
                results.entry(job.to_owned()).or_default();
            } else if let Some(what) = line.strip_prefix("swept ") {
                swept.push(what.to_owned());
            } else if let Some(zone) = line.strip_prefix("zone ")
                && side.role == Role::Passive
            {
                lock(&self.state).zone = parse_zone(zone);
            }
        }
        lock(&self.state)
            .before
            .insert(side.name, Footprint::parse(output));
        if !swept.is_empty() || !results.is_empty() {
            tracing::info!(
                server = side.remote.label(),
                ?swept,
                "removed what earlier direct transfers left"
            );
            self.trace.event(
                "direct.sweep",
                json!({
                    "side": side.name,
                    "remote": side.remote.label(),
                    "swept": swept,
                    "results": results,
                }),
            );
        }
    }

    /// The active side logs in to the passive side once with the job's key.
    async fn probe(&self) -> std::result::Result<(), String> {
        let started = Instant::now();
        let probe = self
            .run(
                &self.active,
                &format!("sh {}/driver.sh probe", self.config.dir()),
                None,
            )
            .await
            .map_err(|error| format!("{}: {error}", self.active.remote.label()))?;
        let report = parse_probe(&probe.stdout_text());
        self.coarse_clock.store(
            report["clock-source"] == "epoch-s",
            std::sync::atomic::Ordering::Relaxed,
        );
        {
            let mut state = lock(&self.state);
            state.active_tar = report["tools"]["tar"]
                .as_str()
                .is_some_and(|path| !path.is_empty());
            state.passive_tar = report["peer-tar"]
                .as_str()
                .is_some_and(|path| !path.is_empty());
        }
        self.trace.event(
            "direct.probe",
            json!({
                "ok": probe.success(),
                "ms": started.elapsed().as_secs_f64() * 1000.0,
                "report": report,
            }),
        );
        if probe.success() {
            return Ok(());
        }
        let ssh_error = self.read_artifact(&self.active, "err.probe").await;
        Err(describe_probe_failure(
            self.active.remote.label(),
            self.passive.remote.label(),
            &report,
            ssh_error.as_deref().unwrap_or_default(),
        ))
    }

    /// Notes where each file starts and which batch it is in.
    fn remember(&self, entries: &[Entry], work: &script::Work, staging: &str) {
        let mut state = lock(&self.state);
        state.offsets = entries
            .iter()
            .map(|entry| (entry.index, entry.offset))
            .collect();
        state.batches = work
            .batches
            .iter()
            .map(|batch| batch.iter().map(|entry| entry.index).collect())
            .collect();
        state.batch_of = work
            .batches
            .iter()
            .enumerate()
            .flat_map(|(number, batch)| batch.iter().map(move |entry| (entry.index, number)))
            .collect();
        state.stage = (!work.batches.is_empty()).then(|| staging.to_owned());
    }

    /// Makes the folders for tables and batches.
    async fn make_folders(&self, work: &script::Work) -> std::result::Result<(), String> {
        let mut folders = Vec::new();
        if !work.singles.is_empty() {
            folders.push((&self.passive, "tab"));
        }
        if !work.batches.is_empty() {
            folders.push((&self.active, "batch"));
            folders.push((&self.passive, "batch"));
        }
        for (side, folder) in folders {
            let path = side.path(folder).map_err(|error| error.to_string())?;
            side.remote
                .mkdir(&path)
                .await
                .map_err(|error| format!("{}: {error}", side.remote.label()))?;
        }
        Ok(())
    }

    /// Uploads the driver's lists, the passive side's tables, the batches
    /// and the name of the staging folder in `target_dir`.
    async fn upload_files(
        &self,
        entries: &[Entry],
        target_dir: &RemotePath,
    ) -> std::result::Result<(), String> {
        let started = Instant::now();
        let batching = {
            let state = lock(&self.state);
            self.batching && state.active_tar && state.passive_tar
        };
        let work = script::split_work(
            entries.to_vec(),
            self.config.workers,
            batching,
            self.config.mode,
        );
        let staging = target_dir
            .join(
                &EntryName::new(&format!(".ferge-{}", self.config.job))
                    .map_err(|error| error.to_string())?,
            )
            .as_str()
            .to_owned();
        self.remember(entries, &work, &staging);
        self.make_folders(&work).await?;
        let mut uploads: Vec<(&Side, String, Bytes, u32)> = Vec::new();
        for (worker, list) in script::worker_lists(&work, self.config.workers)
            .into_iter()
            .enumerate()
        {
            uploads.push((
                &self.active,
                format!("list.{worker}"),
                Bytes::from(list),
                0o600,
            ));
        }
        for (number, table) in script::tables(&work.singles) {
            uploads.push((
                &self.passive,
                format!("tab/{number}.sh"),
                Bytes::from(table),
                0o600,
            ));
        }
        for (number, batch) in work.batches.iter().enumerate() {
            for side in [&self.active, &self.passive] {
                uploads.push((
                    side,
                    format!("batch/{number}.sh"),
                    Bytes::from(script::batch_file(batch, self.config.mode, side.role)),
                    0o600,
                ));
            }
        }
        if !work.batches.is_empty() {
            uploads.push((
                self.side(self.config.target_role()),
                "stage".to_owned(),
                Bytes::from(script::stage_file(&staging)),
                0o600,
            ));
        }
        let bytes: usize = uploads
            .iter()
            .map(|(_, _, contents, _)| contents.len())
            .sum();
        let count = uploads.len();
        self.upload_all(uploads).await?;
        self.trace.event(
            "direct.files",
            json!({
                "files": entries.len(),
                "alone": work.singles.len(),
                "batches": work.batches.len(),
                "batched": work.batches.iter().map(Vec::len).sum::<usize>(),
                "batching": batching,
                "uploads": count,
                "bytes": bytes,
                "ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        );
        Ok(())
    }

    /// Starts the driver detached from this session.
    async fn start(&self) -> std::result::Result<(), String> {
        let printed = self
            .require(&self.active, &format!("sh {}/start.sh", self.config.dir()))
            .await?;
        tracing::info!(
            job = %self.config.job,
            active = self.active.remote.label(),
            passive = self.passive.remote.label(),
            mode = self.config.mode.as_str(),
            "direct transfer started"
        );
        self.trace
            .event("direct.start", json!({ "reply": printed.trim() }));
        Ok(())
    }

    /// Follows the job's status until it ends; returns its exit code (-1 if
    /// the driver died without saying so).
    async fn watch(&self, transfer: &Transfer, plan: &Plan) -> Result<i32> {
        let status = self.active.path("status")?;
        let mut read = 0u64;
        let mut text = Vec::new();
        let mut in_flight = InFlight::default();
        let mut checked = Instant::now();
        // Soon after news, then less often while nothing happens.
        let fastest = FIRST_POLL.min(self.poll_interval);
        let mut wait = fastest;
        loop {
            tokio::time::sleep(wait).await;
            let new = read_from(self.active.remote.as_ref(), &status, read).await?;
            let lines = !new.is_empty();
            read += new.len() as u64;
            text.extend_from_slice(&new);
            if let Some(code) = self.apply_lines(&mut text, transfer, plan, &mut in_flight) {
                return Ok(code);
            }
            let grew = self.track(transfer, plan, &in_flight).await;
            wait = if lines || grew {
                fastest
            } else {
                (wait * 2).min(self.poll_interval)
            };
            if checked.elapsed() >= ALIVE_EVERY {
                checked = Instant::now();
                let alive = self
                    .run(
                        &self.active,
                        &format!("sh {}/alive.sh", self.config.dir()),
                        None,
                    )
                    .await?;
                if !alive.stdout_text().contains("alive") {
                    // It may have written its last lines just before ending.
                    let new = read_from(self.active.remote.as_ref(), &status, read).await?;
                    text.extend_from_slice(&new);
                    text.push(b'\n');
                    return Ok(self
                        .apply_lines(&mut text, transfer, plan, &mut in_flight)
                        .unwrap_or(-1));
                }
                let due = lock(&self.state)
                    .renew_at
                    .is_some_and(|at| Instant::now() >= at);
                if due {
                    self.renew().await;
                }
            }
        }
    }

    /// Extends the job key's expiry time while ferge watches.
    async fn renew(&self) {
        let zone = lock(&self.state).zone;
        let until = expiry(self.lifetime, zone);
        let renewed = self
            .script(
                &self.passive,
                "renew",
                &script::renew_script(&self.config.job, &until),
            )
            .await
            .is_ok_and(|output| output.stdout_text().contains("renewed"));
        self.trace
            .event("direct.renew", json!({ "expiry": until, "ok": renewed }));
        lock(&self.state).renew_at = Some(Instant::now() + self.lifetime / 2);
    }

    /// Applies the complete lines in `text`, leaving a partial last line.
    /// Returns the job's exit code once it ended.
    fn apply_lines(
        &self,
        text: &mut Vec<u8>,
        transfer: &Transfer,
        plan: &Plan,
        in_flight: &mut InFlight,
    ) -> Option<i32> {
        while let Some(end) = text.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = text.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]);
            if line.trim().is_empty() {
                continue;
            }
            match Status::parse(&line) {
                Some(status) => {
                    if let Some(code) = self.apply(status, transfer, plan, in_flight) {
                        return Some(code);
                    }
                }
                None => self
                    .trace
                    .event("direct.status.unknown", json!({ "line": line })),
            }
        }
        None
    }

    #[expect(clippy::too_many_lines, reason = "one arm per status line")]
    fn apply(
        &self,
        status: Status,
        transfer: &Transfer,
        plan: &Plan,
        in_flight: &mut InFlight,
    ) -> Option<i32> {
        let pending = |index: usize| {
            plan.files
                .get(index)
                .filter(|file| lock(&file.state).status == FileStatus::Pending)
        };
        // When a file started: alone, or with its batch.
        let started_at = |index: usize, in_flight: &mut InFlight| {
            in_flight.files.remove(&index).or_else(|| {
                let batch = lock(&self.state).batch_of.get(&index).copied()?;
                in_flight.batches.get(&batch).copied()
            })
        };
        let method = |index: usize| {
            if lock(&self.state).batch_of.contains_key(&index) {
                "direct-batch"
            } else {
                "direct"
            }
        };
        match status {
            Status::Begin { time, pid } => self
                .trace
                .event("direct.begin", json!({ "server_t": time, "pid": pid })),
            Status::Connected { ok, time } => self
                .trace
                .event("direct.connected", json!({ "ok": ok, "server_t": time })),
            Status::Started { index, time } => {
                in_flight.files.insert(index, time);
                if let Some(file) = pending(index) {
                    let offset = lock(&self.state).offsets.get(&index).copied().unwrap_or(0);
                    let mut state = lock(&file.state);
                    transfer
                        .progress
                        .add_bytes(i128::from(offset) - i128::from(state.counted));
                    state.counted = offset;
                    state.resume_at = Some(offset);
                    drop(state);
                    transfer.control().current = file.source.file_name().map(str::to_owned);
                    if self.trace.is_enabled() {
                        self.trace.event(
                            "file.start",
                            json!({
                                "i": index,
                                "path": file.source.as_str(),
                                "offset": offset,
                                "server_t": time,
                            }),
                        );
                    }
                }
            }
            Status::BatchStarted { batch, time } => {
                in_flight.batches.insert(batch, time);
                let files = lock(&self.state)
                    .batches
                    .get(batch)
                    .cloned()
                    .unwrap_or_default();
                if let Some(file) = files.iter().find_map(|index| pending(*index)) {
                    transfer.control().current = file.source.file_name().map(str::to_owned);
                }
                self.trace.event(
                    "direct.batch.start",
                    json!({ "batch": batch, "files": files.len(), "server_t": time }),
                );
            }
            Status::BatchEnded { batch, code, time } => {
                let started = in_flight.batches.remove(&batch);
                self.trace.event(
                    "direct.batch.end",
                    json!({
                        "batch": batch,
                        "code": code,
                        "ms": self.duration_ms(started, time),
                        "server_t": time,
                    }),
                );
            }
            Status::Done { index, bytes, time } => {
                let started = started_at(index, in_flight);
                if let Some(file) = pending(index) {
                    {
                        let mut state = lock(&file.state);
                        transfer
                            .progress
                            .add_bytes(i128::from(state.size) - i128::from(state.counted));
                        state.counted = state.size;
                        state.resume_at = Some(state.size);
                    }
                    let method = method(index);
                    self.trace.event(
                        "file.done",
                        json!({
                            "i": index,
                            "bytes": bytes,
                            "ms": self.duration_ms(started, time),
                            "method": method,
                            "server_t": time,
                        }),
                    );
                    copy::finish_file(transfer, file, Ok(FileOutcome::Copied(method)));
                }
            }
            Status::Failed {
                index,
                code,
                time,
                reason,
            } => {
                let started = started_at(index, in_flight);
                if let Some(file) = pending(index) {
                    let message = script::explain(&reason, code);
                    self.trace.event(
                        "file.error",
                        json!({
                            "i": index,
                            "code": code,
                            "reason": reason,
                            "error": message,
                            "method": method(index),
                            "ms": self.duration_ms(started, time),
                            "server_t": time,
                        }),
                    );
                    copy::finish_file(
                        transfer,
                        file,
                        Err(Error::Remote {
                            path: file.source.clone(),
                            message,
                        }),
                    );
                }
            }
            Status::End { code, time } => {
                self.trace
                    .event("direct.end", json!({ "code": code, "server_t": time }));
                return Some(code);
            }
        }
        None
    }

    /// Milliseconds between two driver times, if its clock is fine enough.
    fn duration_ms(&self, started: Option<f64>, ended: f64) -> Option<f64> {
        if self.coarse_clock.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        started.map(|started| ((ended - started) * 1000.0).max(0.0))
    }

    /// Counts the bytes of files in flight from their partial files; whether
    /// any grew.
    async fn track(&self, transfer: &Transfer, plan: &Plan, in_flight: &InFlight) -> bool {
        let mut grew = false;
        for index in in_flight.files.keys() {
            let Some(file) = plan.files.get(*index) else {
                continue;
            };
            let destination = {
                let state = lock(&file.state);
                if state.status != FileStatus::Pending {
                    continue;
                }
                state.destination.clone()
            };
            let Some(part) = destination.and_then(|(path, _)| part_path(&path)) else {
                continue;
            };
            if let Ok(metadata) = transfer.target().stat(&part).await {
                let mut state = lock(&file.state);
                if state.status == FileStatus::Pending {
                    let bytes = metadata.size.min(state.size);
                    grew |= bytes > state.counted;
                    transfer
                        .progress
                        .add_bytes(i128::from(bytes) - i128::from(state.counted));
                    state.counted = bytes;
                }
            }
        }
        grew
    }

    /// Stops a running job and cleans up. Partial files stay.
    pub(super) async fn stop(&self) {
        let _ = self
            .run(
                &self.active,
                &format!("sh {}/stop.sh", self.config.dir()),
                None,
            )
            .await;
        self.finish().await;
    }

    /// Records what the job left on the active side, then removes all the
    /// job added on both sides (its key, folders, staging folder) unless the
    /// folders are kept, and checks that each server is left as found.
    /// Safe to call twice.
    async fn finish(&self) {
        let mut finished = self.finished.lock().await;
        if *finished {
            return;
        }
        *finished = true;
        let started = Instant::now();
        let mut names = vec![
            "result".to_owned(),
            "status".to_owned(),
            "log".to_owned(),
            "err.probe".to_owned(),
            "err.master".to_owned(),
        ];
        names.extend((0..self.config.workers).map(|worker| format!("err.{worker}")));
        for name in names {
            if let Some(text) = self.read_artifact(&self.active, &name).await {
                self.trace.event(
                    "artifact",
                    json!({ "side": "active", "name": name, "text": cut(&text) }),
                );
            }
        }
        let stage = lock(&self.state).stage.clone();
        let target = self.config.target_role();
        let finish = |side: &Side| script::Finish {
            stage: stage.as_deref().filter(|_| side.role == target),
            unkey: side.role == Role::Passive,
            keep: self.config.keep,
        };
        let active_script = script::finish_script(&self.config, &finish(&self.active));
        let passive_script = script::finish_script(&self.config, &finish(&self.passive));
        let (active, passive) = tokio::join!(
            self.script(&self.active, "finish", &active_script),
            self.script(&self.passive, "finish", &passive_script),
        );
        for (side, output) in [(&self.active, active), (&self.passive, passive)] {
            self.check_footprint(side, output);
        }
        let record = lock(&self.state).record.take();
        if let Some(record) = record {
            let _ = tokio::fs::remove_file(&record).await;
        }
        self.trace.event(
            "direct.cleanup",
            json!({
                "job": self.config.job,
                "kept": self.config.keep,
                "ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        );
    }

    /// Writes what ferge needs to follow the job again after it quit.
    async fn save_record(
        &self,
        transfer: &Transfer,
        plan: &Plan,
        entries: &[Entry],
        dir: Option<&std::path::Path>,
    ) {
        let server = |remote: &dyn Remote| {
            let endpoint = remote.server_side()?.endpoint();
            Some(RecordedServer {
                alias: endpoint.alias.clone(),
                user: endpoint.user.clone(),
                port: endpoint.port,
                label: remote.label().to_owned(),
            })
        };
        let (Some(source), Some(target)) = (
            server(transfer.source().as_ref()),
            server(transfer.target().as_ref()),
        ) else {
            return;
        };
        let files = entries
            .iter()
            .filter_map(|entry| {
                let file = plan.files.get(entry.index)?;
                let (destination, _) = lock(&file.state).destination.clone()?;
                Some(RecordedFile {
                    index: entry.index,
                    source: file.source.clone(),
                    destination,
                    size: entry.size,
                    replace: entry.replace,
                    offset: entry.offset,
                })
            })
            .collect();
        let record = {
            let state = lock(&self.state);
            let before = |side: &str| {
                state
                    .before
                    .get(side)
                    .map(Footprint::to_text)
                    .unwrap_or_default()
            };
            JobRecord {
                job: self.config.job.clone(),
                title: transfer.title.clone(),
                source,
                target,
                target_dir: transfer.target_dir.clone(),
                items: transfer.items.clone(),
                started: jiff::Timestamp::now().to_string(),
                mode: self.config.mode,
                workers: self.config.workers,
                keep: self.config.keep,
                linger: self.config.linger,
                lifetime_s: self.lifetime.as_secs(),
                zone: state.zone,
                stage: state.stage.clone(),
                before_active: before(self.active.name),
                before_passive: before(self.passive.name),
                files,
                batches: state.batches.clone(),
            }
        };
        lock(&self.state).job_record = Some(record.clone());
        let Some(dir) = dir else {
            return;
        };
        match record::save(dir, &record).await {
            Ok(path) => {
                self.trace.event(
                    "direct.record",
                    json!({ "file": path.display().to_string() }),
                );
                lock(&self.state).record = Some(path);
            }
            Err(error) => {
                tracing::warn!(%error, "cannot record the direct job; ferge will not follow it after a restart");
                self.trace
                    .event("direct.record", json!({ "error": error.to_string() }));
            }
        }
    }

    /// The job of `record` again, on the transfer's servers.
    async fn reattach(
        record: &JobRecord,
        transfer: &Transfer,
        shared: &Shared,
    ) -> Result<Arc<Self>> {
        let (active, passive) = sides_of(transfer, record.mode);
        let config = Config {
            job: record.job.clone(),
            mode: record.mode,
            peer_host: String::new(),
            peer_port: 0,
            peer_user: String::new(),
            workers: record.workers,
            keep: record.keep,
            verbose: false,
            linger: record.linger,
        };
        let dir = |home: RemotePath| {
            RemotePath::parse(&format!(
                "{}/{}",
                home.as_str().trim_end_matches('/'),
                config.dir()
            ))
        };
        let active_dir = dir(active.home_dir().await?)?;
        let passive_dir = dir(passive.home_dir().await?)?;
        let mut before = HashMap::new();
        before.insert("active", Footprint::parse(&record.before_active));
        before.insert("passive", Footprint::parse(&record.before_passive));
        let batch_of = record
            .batches
            .iter()
            .enumerate()
            .flat_map(|(number, batch)| batch.iter().map(move |index| (*index, number)))
            .collect();
        let state = JobState {
            before,
            zone: record.zone,
            stage: record.stage.clone(),
            offsets: record
                .files
                .iter()
                .map(|file| (file.index, file.offset))
                .collect(),
            batches: record.batches.clone(),
            batch_of,
            // Renew at the first check: ferge was away for a while.
            renew_at: Some(Instant::now()),
            record: shared
                .options
                .direct
                .records
                .as_ref()
                .map(|dir| record::path(dir, &record.job)),
            ..JobState::default()
        };
        Ok(Arc::new(Self {
            active: Side {
                name: "active",
                role: Role::Active,
                remote: Arc::clone(&active),
                dir: active_dir,
            },
            passive: Side {
                name: "passive",
                role: Role::Passive,
                remote: Arc::clone(&passive),
                dir: passive_dir,
            },
            trace: transfer.trace.clone(),
            poll_interval: shared.options.direct.poll_interval,
            batching: true,
            lifetime: Duration::from_secs(record.lifetime_s),
            coarse_clock: std::sync::atomic::AtomicBool::new(false),
            finished: tokio::sync::Mutex::new(false),
            state: std::sync::Mutex::new(state),
            config,
        }))
    }

    /// Compares a side's state after the job with before, and reports it.
    fn check_footprint(&self, side: &Side, output: Result<CommandOutput>) {
        let label = side.remote.label();
        let output = match output {
            Ok(output) if output.success() => output.stdout_text().into_owned(),
            Ok(output) => {
                let error = output.stderr_text().trim().to_owned();
                tracing::warn!(server = label, %error, "cleaning up a direct transfer failed");
                self.trace.event(
                    "direct.footprint",
                    json!({ "side": side.name, "remote": label, "error": error }),
                );
                return;
            }
            Err(error) => {
                tracing::warn!(server = label, %error, "cleaning up a direct transfer failed");
                self.trace.event(
                    "direct.footprint",
                    json!({ "side": side.name, "remote": label, "error": error.to_string() }),
                );
                return;
            }
        };
        if self.config.keep {
            self.trace.event(
                "direct.footprint",
                json!({ "side": side.name, "remote": label, "kept": true }),
            );
            return;
        }
        let after = Footprint::parse(&output);
        let mut differences = lock(&self.state)
            .before
            .get(side.name)
            .map(|before| before.differences(&after))
            .unwrap_or_default();
        differences.extend(
            output
                .lines()
                .filter_map(|line| line.strip_prefix("left "))
                .map(|what| format!("the job's {what} is left")),
        );
        let as_found = differences.is_empty();
        if as_found {
            tracing::info!(server = label, job = %self.config.job, "server left as found");
        } else {
            tracing::warn!(
                server = label,
                job = %self.config.job,
                ?differences,
                "server not left as found"
            );
        }
        self.trace.event(
            "direct.footprint",
            json!({
                "side": side.name,
                "remote": label,
                "as_found": as_found,
                "differences": differences,
            }),
        );
    }

    /// The text of a file in the job folder, if it exists.
    async fn read_artifact(&self, side: &Side, name: &str) -> Option<String> {
        let path = side.path(name).ok()?;
        let mut stream = side.remote.read(&path).await.ok()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() >= ARTIFACT_LIMIT {
                break;
            }
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// The files of a job besides its key, lists and batches: `(on the active
/// side, name, text, mode)`.
fn scripts(
    config: &Config,
    host_key: &str,
    public_key: &str,
    expiry: &str,
) -> Vec<(bool, &'static str, String, u32)> {
    vec![
        (true, "known_hosts", script::known_hosts(host_key), 0o600),
        (true, "lib.sh", script::lib_sh(config), 0o600),
        (true, "peer.sh", script::peer_sh(config), 0o600),
        (true, "driver.sh", script::driver_sh(config), 0o700),
        (true, "start.sh", script::start_sh(config), 0o700),
        (true, "stop.sh", script::stop_sh(config), 0o700),
        (true, "alive.sh", script::alive_sh(config), 0o700),
        (false, "lib.sh", script::lib_sh(config), 0o600),
        (false, "passive.sh", script::passive_sh(config), 0o700),
        (false, "install.sh", script::install_sh(config), 0o700),
        (false, "uninstall.sh", script::uninstall_sh(config), 0o700),
        (
            false,
            "authorized_key",
            script::authorized_key(config, public_key, expiry),
            0o600,
        ),
    ]
}

/// What ferge left on a server, one item per line: its jobs (`job <id>
/// running|stopped|ended|answering <time>`), their staging folders, job
/// keys (`key <id> <expiry>`), a lock, the folder `~/.cache/ferge`.
/// Changes nothing.
pub async fn inspect(remote: &dyn Remote) -> Result<String> {
    run_tool(remote, &script::inspect_script(), "inspected").await
}

/// Stops every ferge job on a server and removes all ferge added for them:
/// keys, job folders, staging folders, the folders it made. Returns what it
/// did, one item per line.
pub async fn purge(remote: &dyn Remote) -> Result<String> {
    run_tool(remote, &script::purge_script(), "purged").await
}

async fn run_tool(remote: &dyn Remote, text: &str, done: &str) -> Result<String> {
    let server = remote
        .server_side()
        .filter(|server| server.runs_commands())
        .ok_or(Error::Unsupported("running commands"))?;
    let output = server
        .run("sh -s", Some(text.as_bytes()), COMMAND_TIMEOUT)
        .await?;
    let printed = output.stdout_text().into_owned();
    if !output.success() || !printed.lines().any(|line| line == done) {
        return Err(Error::Direct(format!(
            "{}: {}",
            remote.label(),
            output.stderr_text().trim()
        )));
    }
    Ok(printed
        .lines()
        .filter(|line| *line != done)
        .fold(String::new(), |mut text, line| {
            let _ = writeln!(text, "{line}");
            text
        }))
}

/// What a file holds from `offset` on; nothing if it does not exist yet.
async fn read_from(remote: &dyn Remote, path: &RemotePath, offset: u64) -> Result<Vec<u8>> {
    let mut stream = match remote.read_from(path, offset).await {
        Ok(stream) => stream,
        Err(Error::NotFound { .. }) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(Error::NotFound { .. }) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
    }
    Ok(bytes)
}

/// The probe's `key value` lines as an object; `tool` lines are collected,
/// and `peer-tar` is taken from the passive side's greeting.
fn parse_probe(output: &str) -> serde_json::Value {
    let mut report = serde_json::Map::new();
    let mut tools = serde_json::Map::new();
    let mut clocks = Vec::new();
    for line in output.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "tool" => {
                let (tool, path) = value.split_once(' ').unwrap_or((value, ""));
                tools.insert(tool.to_owned(), json!(path));
            }
            "clock" => clocks.push(value.parse::<f64>().ok()),
            key => {
                if key == "peer-reply"
                    && let Some(tar) = value
                        .split_whitespace()
                        .find_map(|word| word.strip_prefix("tar="))
                {
                    report.insert("peer-tar".to_owned(), json!(tar));
                }
                report.insert(key.to_owned(), json!(value));
            }
        }
    }
    if let [Some(before), Some(after)] = clocks[..] {
        report.insert("login_ms".to_owned(), json!((after - before) * 1000.0));
    }
    report.insert("tools".to_owned(), serde_json::Value::Object(tools));
    serde_json::Value::Object(report)
}

/// Tools a job cannot do without; the others are used when present.
const REQUIRED_TOOLS: &[&str] = &[
    "ssh", "cat", "tail", "dd", "ls", "ln", "mv", "touch", "date",
];

/// Why the probe failed, in words.
fn describe_probe_failure(
    active: &str,
    passive: &str,
    report: &serde_json::Value,
    ssh_error: &str,
) -> String {
    let missing: Vec<&str> = report["tools"]
        .as_object()
        .map(|tools| {
            tools
                .iter()
                .filter(|(name, path)| {
                    path.as_str().is_none_or(str::is_empty)
                        && REQUIRED_TOOLS.contains(&name.as_str())
                })
                .map(|(name, _)| name.as_str())
                .collect()
        })
        .unwrap_or_default();
    if !missing.is_empty() {
        return format!("{active} lacks {}", missing.join(", "));
    }
    let ssh_error = ssh_error.trim();
    let detail = if ssh_error.is_empty() {
        report["peer-reply"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned()
    } else {
        ssh_error.lines().last().unwrap_or_default().to_owned()
    };
    let hint = if ssh_error.contains("Permission denied") {
        " (the job key was not accepted; the server may read keys from elsewhere \
         than ~/.ssh/authorized_keys or not support expiry-time)"
    } else if ssh_error.contains("Host key verification failed")
        || ssh_error.contains("REMOTE HOST IDENTIFICATION")
    {
        " (it reaches a machine with another host key under that address)"
    } else if ssh_error.contains("timed out")
        || ssh_error.contains("Connection refused")
        || ssh_error.contains("No route")
        || ssh_error.contains("Could not resolve")
    {
        " (a firewall or the network keeps them apart)"
    } else {
        ""
    };
    format!("{active} could not log in to {passive}: {detail}{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_addresses() {
        assert_eq!(
            split_address("example.org:22"),
            Some(("example.org".into(), 22))
        );
        assert_eq!(split_address("[::1]:2222"), Some(("::1".into(), 2222)));
        assert_eq!(split_address("example.org"), None);
    }

    #[test]
    fn job_ids_are_plain() {
        let id = job_id().unwrap();
        assert_eq!(id.len(), 17);
        assert!(script::is_plain(&id));
    }

    #[test]
    fn generates_openssh_keys() {
        let (private, public) = generate_key("j00").unwrap();
        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(public.starts_with("ssh-ed25519 AAAA"));
    }

    #[test]
    fn formats_touch_times() {
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_577_979_045);
        assert_eq!(touch_time(time).as_deref(), Some("202001021530.45"));
    }

    #[test]
    fn keys_last_long_enough_in_the_servers_time() {
        let hour = Duration::from_hours(1);
        assert_eq!(key_lifetime(2 * hour, 0), 2 * hour);
        assert_eq!(key_lifetime(2 * hour, 3_600_000_000), 3 * hour);
        assert_eq!(key_lifetime(2 * hour, u64::MAX), KEY_LIFETIME_MAX);
        assert_eq!(parse_zone("+0130"), Some(5400));
        assert_eq!(parse_zone("-0800"), Some(-28_800));
        assert_eq!(parse_zone("CET"), None);
        // Two hours from now, read in UTC+2, is four hours ahead of UTC.
        let utc = expiry(2 * hour, Some(0));
        let east = expiry(2 * hour, Some(7200));
        let parse = |text: &str| {
            jiff::tz::Offset::UTC
                .to_timestamp(jiff::civil::DateTime::strptime("%Y%m%d%H%M", text).unwrap())
                .unwrap()
        };
        assert_eq!(parse(&east).duration_since(parse(&utc)).as_secs(), 7200);
    }

    #[test]
    fn reports_how_a_server_changed() {
        let before = Footprint::parse(
            "note home 10 1 2\nentry home .\nentry home .cache\nnote cache 5 3 4\n\
             note ssh -\nnote keys 7 5 6\n",
        );
        let same = Footprint::parse(
            "note home 10 1 2\nentry home .cache\nnote cache 5 3 4\nnote ssh -\nnote keys 7 5 6\n",
        );
        assert_eq!(before.differences(&same), Vec::<String>::new());
        let after = Footprint::parse(
            "note home 11 9 9\nentry home .cache\nentry home .ssh\nnote cache 6 3 4\n\
             note ssh 1 2 3\nnote keys 7 8 9\n",
        );
        assert_eq!(
            before.differences(&after),
            vec![
                "~: added .ssh",
                "~/.cache: modification time changed",
                "~/.ssh was added",
                "~/.ssh/authorized_keys: contents changed",
            ]
        );
    }

    #[test]
    fn explains_probe_failures() {
        let report = parse_probe(
            "tool ssh /usr/bin/ssh\ntool setsid \ntool tar \nclock 10.0\nclock 10.5\n\
             peer-code 255\npeer-reply \n",
        );
        assert_eq!(report["login_ms"], 500.0);
        let reason = describe_probe_failure(
            "a",
            "b",
            &report,
            "ssh: connect to host b port 22: Connection refused\n",
        );
        assert!(
            reason.contains("Connection refused") && reason.contains("firewall"),
            "{reason}"
        );
        let report = parse_probe("tool ssh \n");
        assert_eq!(describe_probe_failure("a", "b", &report, ""), "a lacks ssh");
        let report = parse_probe("peer-reply ferge-passive j1 tar=/bin/tar Linux 6.1 x86_64\n");
        assert_eq!(report["peer-tar"], "/bin/tar");
    }
}
