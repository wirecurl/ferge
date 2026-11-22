//! Work a server can do on its own, so file contents need not pass through
//! this computer: copying within the server and running commands, which
//! direct transfers between two servers are built on.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::Result;
use crate::path::RemotePath;

/// How a server is reached and who is logged in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The host as asked for (an `ssh_config` alias, a name or an address),
    /// to connect the same way again.
    pub alias: String,
    /// Host name or address as this computer dials it.
    pub hostname: String,
    pub port: u16,
    pub user: String,
    /// The server's host key in OpenSSH format (`ssh-ed25519 AAAA…`).
    pub host_key: String,
    /// Reached through a `ProxyCommand` or jump host, so the address may mean
    /// nothing to another machine.
    pub proxied: bool,
    /// The SFTP server runs as root (through sudo); commands do not.
    pub elevated: bool,
}

impl Endpoint {
    /// Whether `other` is the same account on the same machine, so that a
    /// path means the same file in both sessions.
    ///
    /// The host key alone is not enough: cloned virtual machines sometimes
    /// share one.
    pub fn same_account(&self, other: &Self) -> bool {
        self.host_key == other.host_key
            && self.hostname.eq_ignore_ascii_case(&other.hostname)
            && self.port == other.port
            && self.user == other.user
            && self.elevated == other.elevated
    }

    /// `host:port`, as used to look up address overrides.
    pub fn address(&self) -> String {
        if self.hostname.contains(':') {
            format!("[{}]:{}", self.hostname, self.port)
        } else {
            format!("{}:{}", self.hostname, self.port)
        }
    }
}

/// What a command printed and how it ended.
#[derive(Clone, Debug, Default)]
pub struct CommandOutput {
    /// `None` when the command was killed by a signal or the channel closed
    /// without an exit status.
    pub status: Option<u32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    pub fn stderr_text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// How [`ServerSide::copy_within`] copied a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMethod {
    /// The SFTP extension `copy-data` (OpenSSH 9.0 and later).
    CopyData,
    /// `cp` run as a command.
    Cp,
}

impl CopyMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CopyData => "copy-data",
            Self::Cp => "cp",
        }
    }
}

/// Reports the confirmed length of a file being copied.
pub type CopyProgress<'a> = &'a (dyn Fn(u64) + Send + Sync);

/// A server that can work on its own. Obtained through
/// [`Remote::server_side`](super::Remote::server_side).
#[async_trait]
pub trait ServerSide: Send + Sync + fmt::Debug {
    fn endpoint(&self) -> &Endpoint;

    /// Whether commands can run for this session. Root sessions say no:
    /// commands would run as the login user, not as root.
    fn runs_commands(&self) -> bool;

    /// Runs `command` through the login shell and feeds it `input`. Fails
    /// with [`Error::Unsupported`](crate::Error::Unsupported) if the server
    /// refuses to run commands, and with a timeout after `timeout`.
    async fn run(
        &self,
        command: &str,
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<CommandOutput>;

    /// Copies `from` to `to` on the server. `to` is opened for writing at
    /// `offset`: truncated at 0, otherwise its first `offset` bytes are kept
    /// and the copy continues from there. `progress` receives the confirmed
    /// length of `to` as the copy advances. Returns the final length.
    async fn copy_within(
        &self,
        from: &RemotePath,
        to: &RemotePath,
        offset: u64,
        progress: CopyProgress<'_>,
    ) -> Result<(u64, CopyMethod)>;
}
