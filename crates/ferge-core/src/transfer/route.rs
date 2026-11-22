//! Which way file contents take, and the options that steer it.
//!
//! - **Relay**: through this computer, read from the source and written to
//!   the target. Always possible.
//! - **Server**: source and target are the same account on the same server,
//!   which copies by itself (`copy-data`, else `cp`).
//! - **Direct**: two servers exchange the data over their own SSH connection
//!   (see [`direct`](super::direct)): the source pushes to the target, or the
//!   target pulls from the source, whichever can reach the other.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// The way a transfer's data takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Through this computer.
    Relay,
    /// Within one server.
    Server,
    /// The source sends to the target.
    DirectPush,
    /// The target fetches from the source.
    DirectPull,
}

impl Route {
    /// Whether the data bypasses this computer.
    pub fn is_direct(self) -> bool {
        matches!(self, Self::DirectPush | Self::DirectPull)
    }

    /// Whether the copy goes on when ferge quits or loses the connection.
    pub fn runs_without_ferge(self) -> bool {
        self.is_direct()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            Self::Server => "server",
            Self::DirectPush => "direct-push",
            Self::DirectPull => "direct-pull",
        }
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which routes a queue may use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoutePolicy {
    /// The fastest route that works, falling back to the relay.
    #[default]
    Auto,
    /// Always through this computer, as before direct transfers existed.
    RelayOnly,
    /// Direct or within the server; fail instead of falling back.
    DirectOnly,
}

/// Settings of direct transfers.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches, mostly for debugging"
)]
pub struct DirectOptions {
    /// Files a job copies at the same time.
    pub workers: usize,
    /// Try pulling (the target dials the source) before pushing.
    pub prefer_pull: bool,
    /// Leave the job folders on the servers for inspection. The job's key is
    /// removed all the same.
    pub keep_jobs: bool,
    /// Let `ssh` on the server log its debug output into the job folder.
    pub ssh_verbose: bool,
    /// Addresses under which a server is reached from the other one, when
    /// they differ from what this computer dials: `(from, to)` pairs of
    /// `host:port`, e.g. a private network between two virtual machines.
    pub addresses: Vec<(String, String)>,
    /// Send small files in batches, one stream each (needs `tar` on both
    /// servers).
    pub batching: bool,
    /// How long a job's key lasts at least; longer for many bytes. ferge
    /// renews it while it watches the job.
    pub key_lifetime: Duration,
    /// How often ferge reads a running job's progress at least (more often
    /// while it changes).
    pub poll_interval: Duration,
    /// How long a job that ended waits for ferge to collect it; then it
    /// tidies itself, keeping a short result for the next job to report.
    pub linger: Duration,
    /// Where ferge records running jobs, to follow them again after it
    /// quit (see [`direct::recorded_jobs`](super::direct::recorded_jobs));
    /// `None` records nothing.
    pub records: Option<PathBuf>,
}

impl Default for DirectOptions {
    fn default() -> Self {
        Self {
            workers: 4,
            prefer_pull: false,
            keep_jobs: false,
            ssh_verbose: false,
            addresses: Vec::new(),
            batching: true,
            key_lifetime: Duration::from_hours(2),
            poll_interval: Duration::from_millis(250),
            linger: Duration::from_mins(2),
            records: None,
        }
    }
}

impl DirectOptions {
    /// The address under which the other server reaches `address`.
    pub(crate) fn address_for(&self, address: &str) -> String {
        self.addresses
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(address))
            .map_or_else(|| address.to_owned(), |(_, to)| to.clone())
    }

    pub(crate) fn has_override(&self, address: &str) -> bool {
        self.addresses
            .iter()
            .any(|(from, _)| from.eq_ignore_ascii_case(address))
    }
}

/// Settings of a [`TransferQueue`](super::TransferQueue).
#[derive(Clone, Debug)]
pub struct QueueOptions {
    /// Files copied at the same time, across all relayed transfers.
    pub parallel_files: usize,
    pub route: RoutePolicy,
    pub direct: DirectOptions,
    /// Where every transfer records a trace (see [`trace`](super::trace));
    /// `None` records nothing.
    pub trace_dir: Option<PathBuf>,
}

impl QueueOptions {
    pub fn new(parallel_files: usize) -> Self {
        Self {
            parallel_files,
            route: RoutePolicy::Auto,
            direct: DirectOptions::default(),
            trace_dir: None,
        }
    }

    /// Applies the debugging overrides from the environment:
    ///
    /// - `FERGE_ROUTE=auto|relay|direct`
    /// - `FERGE_TRACE_DIR=<dir>` records traces there; `FERGE_TRACE=0` turns
    ///   recording off
    /// - `FERGE_DIRECT_WORKERS=<n>`, `FERGE_DIRECT_PREFER=pull`,
    ///   `FERGE_DIRECT_KEEP=1`, `FERGE_DIRECT_VERBOSE=1`,
    ///   `FERGE_DIRECT_BATCH=0` (every file alone)
    /// - `FERGE_DIRECT_ADDRESS=<host:port>=<host:port>[,…]`
    ///
    /// Unknown values are logged and ignored.
    #[must_use]
    pub fn with_env(mut self) -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        match var("FERGE_ROUTE").as_deref() {
            None => {}
            Some("auto") => self.route = RoutePolicy::Auto,
            Some("relay") => self.route = RoutePolicy::RelayOnly,
            Some("direct") => self.route = RoutePolicy::DirectOnly,
            Some(other) => {
                tracing::warn!(value = other, "FERGE_ROUTE: expected auto, relay or direct");
            }
        }
        if let Some(dir) = var("FERGE_TRACE_DIR") {
            self.trace_dir = Some(PathBuf::from(dir));
        }
        if var("FERGE_TRACE").as_deref() == Some("0") {
            self.trace_dir = None;
        }
        if let Some(workers) = var("FERGE_DIRECT_WORKERS") {
            match workers.parse::<usize>() {
                Ok(workers) if (1..=32).contains(&workers) => self.direct.workers = workers,
                _ => tracing::warn!(value = workers, "FERGE_DIRECT_WORKERS: expected 1 to 32"),
            }
        }
        if let Some(prefer) = var("FERGE_DIRECT_PREFER") {
            self.direct.prefer_pull = prefer == "pull";
        }
        self.direct.keep_jobs |= var("FERGE_DIRECT_KEEP").as_deref() == Some("1");
        self.direct.ssh_verbose |= var("FERGE_DIRECT_VERBOSE").as_deref() == Some("1");
        if var("FERGE_DIRECT_BATCH").as_deref() == Some("0") {
            self.direct.batching = false;
        }
        if let Some(addresses) = var("FERGE_DIRECT_ADDRESS") {
            for pair in addresses.split(',') {
                match pair.split_once('=') {
                    Some((from, to)) if !from.is_empty() && !to.is_empty() => self
                        .direct
                        .addresses
                        .push((from.trim().to_owned(), to.trim().to_owned())),
                    _ => tracing::warn!(value = pair, "FERGE_DIRECT_ADDRESS: expected from=to"),
                }
            }
        }
        tracing::debug!(options = ?self, "transfer options");
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_addresses() {
        let options = DirectOptions {
            addresses: vec![("127.0.0.1:2223".into(), "sshd2:22".into())],
            ..DirectOptions::default()
        };
        assert_eq!(options.address_for("127.0.0.1:2223"), "sshd2:22");
        assert_eq!(options.address_for("example.org:22"), "example.org:22");
        assert!(options.has_override("127.0.0.1:2223"));
    }
}
