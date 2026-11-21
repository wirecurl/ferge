//! SFTP over SSH, built on `russh` and `russh-sftp`.
//!
//! One SSH connection carries several SFTP channels: a metadata channel for
//! listings and directory operations, and lazily opened data channels for file
//! contents. Transfers therefore never delay browsing, and parallel transfers
//! do not share one channel's flow-control window.
//!
//! [`SftpRemote::elevate`] opens another set of channels on the same
//! connection whose SFTP server runs as root (see [`elevate`]).

mod auth;
mod config;
mod elevate;
mod handler;
mod io;
mod known_hosts;

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream::FuturesOrdered;
use futures::{FutureExt as _, StreamExt as _};
use russh::client;
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use tokio::sync::{OnceCell, mpsc};

pub(crate) use self::config::DEFAULT_PORT;
use self::config::ResolvedTarget;
pub use self::config::{KnownHostsSource, SftpOptions, SshConfigSource, configured_hosts};
use self::handler::{ClientHandler, HostKeyFailure};
use self::io::ChunkSizes;
use self::known_hosts::{KnownHosts, host_key_name};
use super::{ByteStream, FileWriter, ListStream, Remote};
use crate::entry::{Entry, Metadata};
use crate::error::{ElevationError, Error, Result};
use crate::interaction::{Account, Interaction};
use crate::path::{EntryName, RemotePath};

/// SFTP protocol version spoken by OpenSSH and supported by russh-sftp.
const SFTP_VERSION: u32 = 3;

/// READDIR requests kept in flight per listing. Servers answer each with the
/// next batch (OpenSSH: up to 100 names), so pipelining hides the round-trip
/// time that otherwise dominates large listings over the internet.
const READDIR_PIPELINE: usize = 8;

/// Seconds after which a single SFTP request is considered lost.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Data channels per connection; transfers are spread across them.
const DATA_CHANNELS: usize = 3;

/// OpenSSH extension that renames over an existing target atomically.
const POSIX_RENAME: &str = "posix-rename@openssh.com";

/// An SSH channel that carries (or will carry) an SFTP session.
type SftpChannel = russh::Channel<client::Msg>;

/// An authenticated SFTP session.
#[derive(Clone)]
pub struct SftpRemote {
    inner: Arc<Inner>,
}

struct Inner {
    label: String,
    account: Account,
    home: RemotePath,
    /// Shared with the root session opened from this one, if any.
    ssh: Arc<client::Handle<ClientHandler>>,
    /// The SFTP server runs as root (through sudo).
    elevated: bool,
    /// Channel for listings and metadata.
    meta: RawSftpSession,
    /// Whether the server supports [`POSIX_RENAME`].
    posix_rename: bool,
    /// Channels for file contents, opened on first use.
    data: [OnceCell<DataChannel>; DATA_CHANNELS],
    next_data: AtomicUsize,
}

/// An SFTP channel for file contents and the chunk sizes it allows.
struct DataChannel {
    session: Arc<RawSftpSession>,
    chunks: ChunkSizes,
}

impl fmt::Debug for SftpRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpRemote")
            .field("label", &self.inner.label)
            .finish_non_exhaustive()
    }
}

impl SftpRemote {
    /// The authenticated account, e.g. to save its password. A root session
    /// keeps the account it was opened from.
    pub fn account(&self) -> &Account {
        &self.inner.account
    }

    /// Whether the SFTP server of this session runs as root.
    pub fn is_elevated(&self) -> bool {
        self.inner.elevated
    }

    /// Connects, verifies the host key, authenticates and opens the SFTP subsystem.
    ///
    /// Questions (unknown host key, passphrases, passwords) go to `interaction`.
    /// Dropping the returned future aborts the attempt.
    pub async fn connect(options: SftpOptions, interaction: Arc<dyn Interaction>) -> Result<Self> {
        let resolve_options = options.clone();
        let target = tokio::task::spawn_blocking(move || config::resolve(&resolve_options))
            .await
            .map_err(|error| Error::protocol("resolving the SSH configuration failed", error))??;
        tracing::info!(
            alias = %target.alias,
            hostname = %target.hostname,
            port = target.port,
            user = %target.user,
            proxy = target.proxy_command.is_some(),
            "connecting"
        );

        let stream = open_transport(&target, options.connect_timeout).await?;
        let failure = HostKeyFailure::default();
        let handler = ClientHandler::new(
            // Like OpenSSH, a HostKeyAlias is used verbatim, without the port.
            target
                .host_key_alias
                .clone()
                .unwrap_or_else(|| host_key_name(&target.hostname, target.port)),
            KnownHosts {
                user_files: target.user_known_hosts.clone(),
                global_files: target.global_known_hosts.clone(),
            },
            Arc::clone(&interaction),
            Arc::clone(&failure),
        );
        let mut ssh = client::connect_stream(Arc::new(ssh_config()), stream, handler)
            .await
            .map_err(|error| handshake_error(error, &failure))?;

        auth::Authenticator::new(
            &mut ssh,
            &target,
            options.password.as_ref(),
            options.use_agent,
            &interaction,
        )
        .run()
        .await?;

        let (meta, extensions) = start_session(open_sftp_channel(&ssh).await?).await?;
        let home = canonical(&meta, ".").await?;
        tracing::info!(remote = %target.label(), %home, "SFTP session ready");
        Ok(Self {
            inner: Arc::new(Inner {
                label: target.label(),
                account: target.account(),
                home,
                ssh: Arc::new(ssh),
                elevated: false,
                meta,
                posix_rename: extensions.contains_key(POSIX_RENAME),
                data: std::array::from_fn(|_| OnceCell::new()),
                next_data: AtomicUsize::new(0),
            }),
        })
    }

    /// Opens a session on the same connection whose SFTP server runs as root,
    /// through sudo. Asks `interaction` for the sudo password if sudo needs
    /// one; nothing has to be configured on the server.
    ///
    /// The new session is independent: dropping it (or this one) leaves the
    /// other working. Its label names root; its account stays the login's.
    pub async fn elevate(&self, interaction: Arc<dyn Interaction>) -> Result<Self> {
        if self.inner.elevated {
            return Ok(self.clone());
        }
        let ssh = &self.inner.ssh;
        let account = &self.inner.account;
        let host = account.host.clone();
        let command = elevate::prepare(ssh, &host, |retry| {
            let interaction = Arc::clone(&interaction);
            async move { interaction.sudo_password(account, retry).await }
        })
        .await?;
        let not_started = |error: Error| match error {
            Error::Protocol { source, .. } => Error::Elevation(ElevationError::NotStarted {
                host: host.clone(),
                source,
            }),
            other => other,
        };
        // Every channel opens now: the password is not kept for later.
        let open = || async {
            let channel = command.open(ssh, &host).await?;
            start_session(channel).await.map_err(not_started)
        };
        let (meta, data) = futures::try_join!(
            open(),
            futures::future::try_join_all((0..DATA_CHANNELS).map(|_| async {
                let (session, extensions) = open().await?;
                Ok::<_, Error>(DataChannel::new(session, &extensions).await)
            })),
        )?;
        drop(command);
        let (meta, extensions) = meta;
        let home = canonical(&meta, ".").await.map_err(not_started)?;
        tracing::info!(remote = %self.inner.label, "SFTP session as root ready");
        let mut data = data.into_iter();
        Ok(Self {
            inner: Arc::new(Inner {
                label: Account {
                    user: "root".to_owned(),
                    ..account.clone()
                }
                .to_string(),
                account: account.clone(),
                home,
                ssh: Arc::clone(ssh),
                elevated: true,
                meta,
                posix_rename: extensions.contains_key(POSIX_RENAME),
                data: std::array::from_fn(|_| OnceCell::new_with(data.next())),
                next_data: AtomicUsize::new(0),
            }),
        })
    }

    /// Closes the connection gracefully. Outstanding operations fail. For a
    /// root session, only its own channels close.
    pub async fn disconnect(&self) {
        let _ = self.inner.meta.close_session();
        if self.inner.elevated {
            for channel in &self.inner.data {
                if let Some(channel) = channel.get() {
                    let _ = channel.session.close_session();
                }
            }
            return;
        }
        let _ = self
            .inner
            .ssh
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }

    /// A data channel; successive calls rotate through [`DATA_CHANNELS`].
    async fn data(&self) -> Result<&DataChannel> {
        let index = self.inner.next_data.fetch_add(1, Ordering::Relaxed) % DATA_CHANNELS;
        self.inner.data[index]
            .get_or_try_init(|| async {
                let channel = open_sftp_channel(&self.inner.ssh).await?;
                let (session, extensions) = start_session(channel).await?;
                Ok(DataChannel::new(session, &extensions).await)
            })
            .await
    }
}

impl DataChannel {
    /// Wraps a fresh session, asking the server for its chunk sizes.
    async fn new(session: RawSftpSession, extensions: &HashMap<String, String>) -> Self {
        let chunks = if extensions.contains_key(russh_sftp::extensions::LIMITS) {
            match session.limits().await {
                Ok(limits) => ChunkSizes::from_limits(limits.max_read_len, limits.max_write_len),
                Err(error) => {
                    tracing::debug!(%error, "reading SFTP limits failed");
                    ChunkSizes::SAFE
                }
            }
        } else {
            ChunkSizes::SAFE
        };
        Self {
            session: Arc::new(session),
            chunks,
        }
    }
}

fn ssh_config() -> client::Config {
    client::Config {
        client_id: russh::SshId::Standard(Cow::Borrowed(concat!(
            "SSH-2.0-ferge_",
            env!("CARGO_PKG_VERSION")
        ))),
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        nodelay: true,
        // Larger windows keep high-latency links busy.
        window_size: 16 * 1024 * 1024,
        ..client::Config::default()
    }
}

async fn open_transport(
    target: &ResolvedTarget,
    timeout: Duration,
) -> Result<russh_config::Stream> {
    let describe = || {
        if target.port == DEFAULT_PORT {
            target.hostname.clone()
        } else {
            format!("{}:{}", target.hostname, target.port)
        }
    };
    if let Some(command) = &target.proxy_command {
        // Like OpenSSH, run the command through the shell so quoting works.
        let exec = format!("exec {command}");
        return russh_config::Stream::proxy_command("/bin/sh", &["-c", &exec])
            .await
            .map_err(|source| Error::Connect {
                target: format!("{} (ProxyCommand)", describe()),
                source,
            });
    }
    let connect = tokio::net::TcpStream::connect((target.hostname.as_str(), target.port));
    let tcp = tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| Error::Timeout("connecting"))?
        .map_err(|source| Error::Connect {
            target: describe(),
            source,
        })?;
    tcp.set_nodelay(true).map_err(|source| Error::Connect {
        target: describe(),
        source,
    })?;
    Ok(russh_config::Stream::Tcp(tcp))
}

fn handshake_error(error: russh::Error, failure: &HostKeyFailure) -> Error {
    if let Some(host_key) = failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        return Error::HostKey(host_key);
    }
    match error {
        russh::Error::IO(source) => Error::Connect {
            target: "SSH server".into(),
            source,
        },
        other => Error::protocol("SSH handshake failed", other),
    }
}

async fn open_sftp_channel(ssh: &client::Handle<ClientHandler>) -> Result<SftpChannel> {
    let channel = ssh
        .channel_open_session()
        .await
        .map_err(|error| Error::protocol("opening an SSH channel failed", error))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| Error::protocol("the server refused the SFTP subsystem", error))?;
    Ok(channel)
}

/// Starts SFTP on `channel`; returns the session with the server's protocol
/// extensions.
async fn start_session(channel: SftpChannel) -> Result<(RawSftpSession, HashMap<String, String>)> {
    let config = russh_sftp::client::Config {
        request_timeout_secs: REQUEST_TIMEOUT_SECS,
        ..russh_sftp::client::Config::default()
    };
    let session = RawSftpSession::new_with_config(channel.into_stream(), config);
    let version = session
        .init()
        .await
        .map_err(|error| Error::protocol("SFTP initialization failed", error))?;
    if version.version != SFTP_VERSION {
        return Err(Error::protocol(
            "unsupported SFTP version",
            format!("server speaks version {}", version.version),
        ));
    }
    Ok((session, version.extensions))
}

async fn canonical(meta: &RawSftpSession, path: &str) -> Result<RemotePath> {
    let name = meta
        .realpath(path)
        .await
        .map_err(|error| Error::protocol("resolving a path failed", error))?;
    let resolved = name
        .files
        .first()
        .ok_or_else(|| Error::protocol("resolving a path failed", "empty REALPATH reply"))?;
    RemotePath::parse(&resolved.filename)
}

/// Maps an SFTP failure for `path` to the shared error type.
fn sftp_error(path: &RemotePath, error: SftpError) -> Error {
    let path = path.clone();
    match error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => Error::NotFound { path },
            StatusCode::PermissionDenied => Error::PermissionDenied { path },
            StatusCode::OpUnsupported => Error::Unsupported("operation rejected by the server"),
            StatusCode::NoConnection | StatusCode::ConnectionLost => Error::Disconnected,
            _ => Error::Remote {
                path,
                message: if status.error_message.is_empty() {
                    status.status_code.to_string()
                } else {
                    status.error_message
                },
            },
        },
        SftpError::Timeout => Error::Timeout("SFTP request"),
        other => Error::Remote {
            path,
            message: other.to_string(),
        },
    }
}

fn is_eof(error: &SftpError) -> bool {
    matches!(error, SftpError::Status(status) if status.status_code == StatusCode::Eof)
}

/// Converts a READDIR batch, dropping `.`, `..` and unsafe names.
fn convert_names(dir: &RemotePath, files: Vec<russh_sftp::protocol::File>) -> Vec<Entry> {
    files
        .into_iter()
        .filter_map(|file| {
            if file.filename == "." || file.filename == ".." {
                return None;
            }
            let Ok(name) = EntryName::new(&file.filename) else {
                tracing::warn!(%dir, name = ?file.filename, "server sent an unsafe file name; skipped");
                return None;
            };
            let attrs = &file.attrs;
            Some(Entry {
                name,
                metadata: Metadata::from_posix(attrs.permissions, attrs.size, attrs.mtime),
            })
        })
        .collect()
}

/// Lists `path` with pipelined READDIR requests and sends each batch to `sender`.
async fn list_task(inner: Arc<Inner>, path: RemotePath, sender: mpsc::Sender<Result<Vec<Entry>>>) {
    let handle = match inner.meta.opendir(path.as_str()).await {
        Ok(handle) => handle.handle,
        Err(error) => {
            let _ = sender.send(Err(sftp_error(&path, error))).await;
            return;
        }
    };

    let read = || inner.meta.readdir(handle.as_str());
    let mut in_flight: FuturesOrdered<_> = (0..READDIR_PIPELINE).map(|_| read()).collect();
    let mut exhausted = false;
    while let Some(result) = futures::select_biased! {
        // Stop as soon as the consumer goes away instead of reading on.
        () = sender.closed().fuse() => None,
        next = in_flight.next().fuse() => next,
    } {
        match result {
            Ok(name) => {
                if !exhausted {
                    in_flight.push_back(read());
                }
                let entries = convert_names(&path, name.files);
                if !entries.is_empty() && sender.send(Ok(entries)).await.is_err() {
                    break;
                }
            }
            // Requests sent after the last batch all report EOF.
            Err(error) if is_eof(&error) => exhausted = true,
            Err(error) => {
                let _ = sender.send(Err(sftp_error(&path, error))).await;
                break;
            }
        }
    }
    drop(in_flight);
    if let Err(error) = inner.meta.close(handle).await {
        tracing::debug!(%path, %error, "closing a directory handle failed");
    }
}

#[async_trait]
impl Remote for SftpRemote {
    fn label(&self) -> &str {
        &self.inner.label
    }

    async fn home_dir(&self) -> Result<RemotePath> {
        Ok(self.inner.home.clone())
    }

    async fn canonicalize(&self, path: &RemotePath) -> Result<RemotePath> {
        let name = self
            .inner
            .meta
            .realpath(path.as_str())
            .await
            .map_err(|error| sftp_error(path, error))?;
        let resolved = name.files.first().ok_or_else(|| Error::Remote {
            path: path.clone(),
            message: "empty REALPATH reply".into(),
        })?;
        RemotePath::parse(&resolved.filename)
    }

    fn list(&self, path: &RemotePath) -> ListStream {
        // The receiver is runtime-agnostic, so the stream can be polled from any
        // executor while the protocol work stays on tokio.
        let (sender, receiver) = mpsc::channel(READDIR_PIPELINE);
        tokio::spawn(list_task(Arc::clone(&self.inner), path.clone(), sender));
        futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        })
        .boxed()
    }

    async fn stat(&self, path: &RemotePath) -> Result<Metadata> {
        let attrs = self
            .inner
            .meta
            .stat(path.as_str())
            .await
            .map_err(|error| sftp_error(path, error))?
            .attrs;
        Ok(Metadata::from_posix(
            attrs.permissions,
            attrs.size,
            attrs.mtime,
        ))
    }

    async fn read_from(&self, path: &RemotePath, offset: u64) -> Result<ByteStream> {
        let channel = self.data().await?;
        Ok(io::read_stream(
            Arc::clone(&channel.session),
            path.clone(),
            offset,
            channel.chunks.read,
        ))
    }

    async fn open_writer(&self, path: &RemotePath, offset: u64) -> Result<Box<dyn FileWriter>> {
        let channel = self.data().await?;
        io::open_writer(
            Arc::clone(&channel.session),
            path,
            offset,
            channel.chunks.write,
        )
        .await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        // SFTPv3 RENAME refuses existing targets, but only with a generic failure
        // status; check first to report it precisely.
        match self.inner.meta.lstat(to.as_str()).await {
            Ok(_) => return Err(Error::AlreadyExists { path: to.clone() }),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {}
            Err(error) => return Err(sftp_error(to, error)),
        }
        self.inner
            .meta
            .rename(from.as_str(), to.as_str())
            .await
            .map(drop)
            .map_err(|error| sftp_error(from, error))
    }

    async fn rename_replace(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        if self.inner.posix_rename {
            let mut data = Vec::with_capacity(8 + from.as_str().len() + to.as_str().len());
            for path in [from.as_str(), to.as_str()] {
                let len = u32::try_from(path.len()).map_err(|_| Error::InvalidPath(path.into()))?;
                data.extend_from_slice(&len.to_be_bytes());
                data.extend_from_slice(path.as_bytes());
            }
            return match self.inner.meta.extended(POSIX_RENAME, data).await {
                Ok(russh_sftp::protocol::Packet::Status(status))
                    if status.status_code == StatusCode::Ok =>
                {
                    Ok(())
                }
                Ok(russh_sftp::protocol::Packet::Status(status)) => {
                    Err(sftp_error(from, SftpError::Status(status)))
                }
                Ok(_) => Err(Error::protocol("renaming failed", "unexpected reply")),
                Err(error) => Err(sftp_error(from, error)),
            };
        }
        // Without the extension, plain SFTPv3 RENAME refuses existing targets.
        match self.inner.meta.remove(to.as_str()).await {
            Ok(_) => {}
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {}
            Err(error) => return Err(sftp_error(to, error)),
        }
        self.inner
            .meta
            .rename(from.as_str(), to.as_str())
            .await
            .map(drop)
            .map_err(|error| sftp_error(from, error))
    }

    async fn set_modified(&self, path: &RemotePath, time: SystemTime) -> Result<()> {
        // SFTPv3 times are 32-bit seconds; dates outside that range are skipped.
        let Some(seconds) = time
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u32::try_from(elapsed.as_secs()).ok())
        else {
            return Ok(());
        };
        let attrs = FileAttributes {
            atime: Some(seconds),
            mtime: Some(seconds),
            ..FileAttributes::empty()
        };
        self.inner
            .meta
            .setstat(path.as_str(), attrs)
            .await
            .map(drop)
            .map_err(|error| sftp_error(path, error))
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .meta
            .remove(path.as_str())
            .await
            .map(drop)
            .map_err(|error| sftp_error(path, error))
    }

    async fn delete_dir(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .meta
            .rmdir(path.as_str())
            .await
            .map(drop)
            .map_err(|error| sftp_error(path, error))
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .meta
            .mkdir(path.as_str(), FileAttributes::empty())
            .await
            .map(drop)
            .map_err(|error| sftp_error(path, error))
    }

    async fn chmod(&self, path: &RemotePath, mode: u32) -> Result<()> {
        let attrs = FileAttributes {
            permissions: Some(mode & 0o7777),
            ..FileAttributes::empty()
        };
        self.inner
            .meta
            .setstat(path.as_str(), attrs)
            .await
            .map(drop)
            .map_err(|error| sftp_error(path, error))
    }
}
