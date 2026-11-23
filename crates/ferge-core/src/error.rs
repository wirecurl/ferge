//! Error types shared by all backends.

use std::io;
use std::path::PathBuf;

use crate::path::RemotePath;

/// Boxed error used to carry backend-specific causes without leaking their types.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Result alias used throughout ferge-core.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while talking to a remote.
///
/// Variants are coarse on purpose: the UI needs to decide how to present a
/// failure (retry, ask for credentials, warn about a host key), not to know which
/// library produced it. The original cause is preserved as [`std::error::Error::source`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A path string was not absolute or contained forbidden characters.
    #[error("invalid path {0:?}")]
    InvalidPath(Box<str>),

    /// A file name was empty, `.`/`..`, or contained `/` or NUL.
    #[error("invalid file name {0:?}")]
    InvalidName(Box<str>),

    /// The path does not exist.
    #[error("{path}: no such file or directory")]
    NotFound { path: RemotePath },

    /// The server or the operating system refused access.
    #[error("{path}: permission denied")]
    PermissionDenied { path: RemotePath },

    /// The target already exists.
    #[error("{path}: already exists")]
    AlreadyExists { path: RemotePath },

    /// Any other failure the remote reported for a specific path.
    #[error("{path}: {message}")]
    Remote { path: RemotePath, message: String },

    /// The backend does not implement the requested operation.
    #[error("not supported: {0}")]
    Unsupported(&'static str),

    /// Establishing the transport (TCP, proxy command) failed.
    #[error("could not connect to {target}")]
    Connect {
        target: String,
        #[source]
        source: io::Error,
    },

    /// The operation did not complete in time.
    #[error("{0} timed out")]
    Timeout(&'static str),

    /// The connection was closed while an operation was in flight.
    #[error("connection closed")]
    Disconnected,

    /// A protocol-level failure (SSH, SFTP) that is not tied to a single path.
    #[error("{context}")]
    Protocol {
        context: &'static str,
        #[source]
        source: BoxError,
    },

    /// The server's host key could not be verified.
    #[error(transparent)]
    HostKey(#[from] HostKeyError),

    /// The server's TLS certificate was not trusted, or an unencrypted
    /// connection was not allowed.
    #[error(transparent)]
    Certificate(#[from] CertificateError),

    /// No offered authentication method was accepted.
    #[error("authentication failed for {user}@{host}")]
    AuthenticationFailed { user: String, host: String },

    /// Working as root on the server was not possible.
    #[error(transparent)]
    Elevation(#[from] ElevationError),

    /// A direct transfer between two servers was not possible or lost
    /// contact with its job.
    #[error("direct transfer: {0}")]
    Direct(String),

    /// The SSH client configuration could not be used.
    #[error("SSH configuration: {0}")]
    Config(String),

    /// A local I/O failure that is not tied to a remote path.
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl Error {
    /// Wraps a protocol error with a short static description of what was attempted.
    pub(crate) fn protocol(context: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Protocol {
            context,
            source: source.into(),
        }
    }

    /// Maps an [`io::Error`] that occurred while accessing `path`.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "reads naturally in `map_err` closures"
    )]
    pub(crate) fn from_io(path: &RemotePath, error: io::Error) -> Self {
        let path = path.clone();
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound { path },
            io::ErrorKind::PermissionDenied => Self::PermissionDenied { path },
            io::ErrorKind::AlreadyExists => Self::AlreadyExists { path },
            _ => Self::Remote {
                path,
                message: error.to_string(),
            },
        }
    }

    /// Whether retrying with other credentials could help.
    /// Whether the connection to the server is gone, so that the same work
    /// may succeed on a new one.
    pub fn is_connection_lost(&self) -> bool {
        matches!(self, Self::Disconnected | Self::Timeout(_))
    }

    pub fn is_authentication(&self) -> bool {
        matches!(self, Self::AuthenticationFailed { .. })
    }

    /// Whether the user called the operation off, e.g. by closing a password
    /// dialog. Nothing needs to be reported.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Elevation(ElevationError::Cancelled))
    }

    /// Whether the server refused access, so working as root could help.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::PermissionDenied { .. })
    }
}

/// Reasons working as root (through `sudo`) was not possible.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ElevationError {
    /// The user did not enter the sudo password.
    #[error("working as root was cancelled")]
    Cancelled,

    /// The server does not run commands for this account (`ForceCommand`,
    /// SFTP-only accounts).
    #[error("{host} does not run commands for this account, which working as root needs")]
    CommandsRefused { host: String },

    /// `sudo` is not installed.
    #[error("sudo is not installed on {host}")]
    NoSudo { host: String },

    /// No `sftp-server` program was found to run as root.
    #[error("{host} has no sftp-server program that could run as root")]
    NoSftpServer { host: String },

    /// `sudo` refused, e.g. because the account may not use it. `message` is
    /// what sudo said.
    #[error("sudo on {host} refused: {message}")]
    Refused { host: String, message: String },

    /// `sudo` accepted, but no SFTP session came up.
    #[error("the SFTP server did not start as root on {host}")]
    NotStarted {
        host: String,
        #[source]
        source: BoxError,
    },
}

/// Reasons a server's host key was not trusted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostKeyError {
    /// The key differs from the one recorded for this host. This is what a
    /// man-in-the-middle attack looks like, so it is never offered for acceptance.
    #[error(
        "the host key for {host} has changed ({fingerprint}); it does not match {} line {line}. \
         Someone could be eavesdropping on the connection. Remove the old entry only if you know \
         why the key changed",
        file.display()
    )]
    Changed {
        host: String,
        fingerprint: String,
        file: PathBuf,
        line: usize,
    },

    /// The key is marked `@revoked` in a known_hosts file.
    #[error("the host key for {host} ({fingerprint}) has been revoked")]
    Revoked { host: String, fingerprint: String },

    /// The user declined an unknown key.
    #[error("the host key for {host} ({fingerprint}) was not accepted")]
    Rejected { host: String, fingerprint: String },

    /// The server presented a host certificate, which is not supported yet.
    #[error("{host} presented a host certificate; host certificates are not supported yet")]
    CertificateUnsupported { host: String },

    /// A known_hosts file exists but could not be read.
    #[error("could not read {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Recording an accepted key failed.
    #[error("could not record the host key in {}", path.display())]
    Save {
        path: PathBuf,
        #[source]
        source: BoxError,
    },
}

/// Reasons a TLS connection was not trusted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CertificateError {
    /// The certificate differs from the one the user chose to trust, and
    /// the system does not trust the new one either. This is what a
    /// man-in-the-middle attack looks like, so it is never offered for
    /// acceptance.
    #[error(
        "the certificate of {server} has changed ({fingerprint}); it does not match {} line \
         {line}. Someone could be eavesdropping on the connection. Remove the old entry only if \
         you know why the certificate changed",
        file.display()
    )]
    Changed {
        server: String,
        fingerprint: String,
        file: PathBuf,
        line: usize,
    },

    /// The user declined a certificate the system does not trust.
    #[error("the certificate of {server} ({fingerprint}) was not accepted: it {problem}")]
    Rejected {
        server: String,
        fingerprint: String,
        problem: String,
    },

    /// The certificate cannot be trusted at all (revoked, broken).
    #[error("the certificate of {server} cannot be trusted: it {problem}")]
    Untrusted { server: String, problem: String },

    /// The server offers no encryption, and the user did not allow an
    /// unencrypted connection.
    #[error("{server} offers no encryption, and an unencrypted connection was not allowed")]
    PlaintextRefused { server: String },

    #[error("could not read {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not record the decision in {}", path.display())]
    Save {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
