//! FTP with TLS (explicit or implicit), through `suppaftp`.
//!
//! An FTP connection carries one transfer at a time, so an [`FtpRemote`]
//! keeps a small pool of logged-in control connections: every listing,
//! download and upload takes one for as long as it runs, and gives it back
//! afterwards. Connections beyond the first are opened on demand, with the
//! security, password and certificate the first one settled on, and stop
//! growing where the server says it has enough.
//!
//! Paths never pass through the server's glob expansion: listings change
//! into the folder and list it without an argument. Whether something
//! exists is found out with `SIZE` and `CWD` (or `MLST`), because servers
//! word their errors as they like.

mod listing;
mod session;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use secrecy::SecretString;
use suppaftp::Status;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use self::listing::Format;
use self::session::{Lease, Pool, Transfer};
use super::{
    ByteStream, Capabilities, FileWriter, LISTING_LIMIT, ListStream, READ_CHUNK, Remote,
    too_many_entries,
};
use crate::destination::{Destination, FtpTarget, FtpTls, Protocol};
use crate::entry::{EntryKind, Metadata};
use crate::error::{Error, Result};
use crate::interaction::{Account, Interaction};
use crate::path::RemotePath;
use crate::tls::TlsOptions;

/// How to connect to an FTP server.
#[derive(Clone, Debug)]
pub struct FtpOptions {
    /// Host name or address.
    pub host: String,
    pub port: Option<u16>,
    /// `anonymous` if unset.
    pub user: Option<String>,
    /// A password to try before asking.
    pub password: Option<SecretString>,
    pub tls: FtpTls,
    pub tls_options: TlsOptions,
    /// Upper bound for establishing the TCP connection.
    pub connect_timeout: Duration,
    /// Upper bound for a command's reply, and for silence on a data
    /// connection.
    pub command_timeout: Duration,
    /// Connections at most. Many servers allow only a few per address;
    /// fewer are used once a server refuses another.
    pub max_connections: usize,
}

impl FtpOptions {
    /// Options for `target` with all defaults.
    pub fn new(target: &FtpTarget) -> Self {
        Self {
            host: target.host.clone(),
            port: target.port,
            user: target.user.clone(),
            password: None,
            tls: target.tls,
            tls_options: TlsOptions::default(),
            connect_timeout: Duration::from_secs(15),
            command_timeout: Duration::from_secs(60),
            max_connections: 6,
        }
    }

    fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.tls.default_port())
    }

    fn account(&self) -> Account {
        Account {
            protocol: Protocol::Ftp,
            user: self.user.clone().unwrap_or_else(|| ANONYMOUS.to_owned()),
            host: self.host.clone(),
            port: self.port(),
        }
    }
}

/// The user name without an account.
const ANONYMOUS: &str = "anonymous";

/// What a server offers, from `FEAT` and `SYST`.
#[derive(Clone, Copy, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent yes/no answers of the server"
)]
struct Features {
    /// `MLSD` and `MLST`.
    mlst: bool,
    /// `MFMT` to set modification times.
    mfmt: bool,
    /// `REST STREAM`: transfers can start at an offset.
    rest: bool,
    /// `EPSV`, passive mode that works across NAT and with IPv6.
    epsv: bool,
    /// `UTF8`: file names are UTF-8 after `OPTS UTF8 ON`.
    utf8: bool,
    /// A Unix server, with Unix permissions.
    unix: bool,
}

/// A connection to an FTP server.
#[derive(Clone, Debug)]
pub struct FtpRemote {
    pool: Arc<Pool>,
}

impl FtpRemote {
    /// Connects and logs in. Questions (certificates, an unencrypted
    /// connection, the password) go to `interaction`.
    pub async fn connect(options: FtpOptions, interaction: Arc<dyn Interaction>) -> Result<Self> {
        let pool = Pool::connect(options, interaction).await?;
        tracing::info!(remote = %pool.label, "FTP connection ready");
        Ok(Self {
            pool: Arc::new(pool),
        })
    }

    /// The account logged in.
    pub fn account(&self) -> Account {
        self.pool.options.account()
    }

    async fn session(&self) -> Result<Lease> {
        Pool::session(&self.pool).await
    }

    /// Metadata of `path`; servers follow symlinks for it.
    async fn metadata(&self, path: &RemotePath) -> Result<Metadata> {
        if path.is_root() {
            return Ok(directory());
        }
        let mut lease = self.session().await?;
        let features = self.pool.features;
        if features.mlst {
            // A refusal may mean that nothing is there, or anything else
            // (ProFTPD: “cannot be listed”); `SIZE` and `CWD` tell.
            match lease.mlst(path).await {
                Ok(line) => {
                    return listing::mlsd(line.trim_start())
                        .map(|(_, metadata)| metadata)
                        .ok_or_else(|| Error::Remote {
                            path: path.clone(),
                            message: format!("the server described it as {line:?}"),
                        });
                }
                Err(error) if is_refusal(&error) => {}
                Err(error) => return Err(error),
            }
        }
        match lease.size(path).await {
            Ok(size) => {
                let modified = lease.mdtm(path).await.ok();
                Ok(Metadata {
                    kind: EntryKind::File,
                    size,
                    modified,
                    permissions: None,
                })
            }
            Err(error) if is_refusal(&error) => match lease.cwd(path).await {
                Ok(()) => Ok(directory()),
                Err(error) if is_refusal(&error) => Err(Error::NotFound { path: path.clone() }),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    /// `error` of an operation on `path`, as [`Error::NotFound`] if `path`
    /// turns out not to exist: servers word that as they like.
    async fn explain(&self, path: &RemotePath, error: Error) -> Error {
        if !is_refusal(&error) {
            return error;
        }
        match self.metadata(path).await {
            Err(Error::NotFound { .. }) => Error::NotFound { path: path.clone() },
            _ => error,
        }
    }

    /// Fails with [`Error::AlreadyExists`] if something is at `path`.
    async fn ensure_free(&self, path: &RemotePath) -> Result<()> {
        match self.metadata(path).await {
            Ok(_) => Err(Error::AlreadyExists { path: path.clone() }),
            Err(Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn directory() -> Metadata {
    Metadata {
        kind: EntryKind::Directory,
        size: 0,
        modified: None,
        permissions: None,
    }
}

/// Whether the server turned the command down (a 5xx reply about a path),
/// as opposed to the connection failing.
fn is_refusal(error: &Error) -> bool {
    matches!(error, Error::Remote { .. } | Error::NotFound { .. })
}

#[async_trait]
impl Remote for FtpRemote {
    fn label(&self) -> &str {
        &self.pool.label
    }

    fn capabilities(&self) -> Capabilities {
        let features = self.pool.features;
        Capabilities {
            resume_writes: features.rest && !self.pool.refuses_resume.load(Ordering::Relaxed),
            atomic_writes: false,
            set_modified: true,
            permissions: features.unix,
        }
    }

    fn destination(&self) -> Option<Destination> {
        let options = &self.pool.options;
        let mut target = FtpTarget::new(&options.host, options.tls).ok()?;
        target.port = Some(options.port());
        target.user.clone_from(&options.user);
        Some(Destination::Ftp(target))
    }

    async fn home_dir(&self) -> Result<RemotePath> {
        Ok(self.pool.home.clone())
    }

    async fn canonicalize(&self, path: &RemotePath) -> Result<RemotePath> {
        let mut lease = self.session().await?;
        lease.cwd(path).await?;
        lease.pwd().await
    }

    fn list(&self, path: &RemotePath) -> ListStream {
        let (sender, receiver) = mpsc::channel(4);
        let remote = self.clone();
        let path = path.clone();
        tokio::spawn(async move {
            if let Err(error) = list_into(&remote, &path, &sender).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        tokio_stream(receiver)
    }

    async fn stat(&self, path: &RemotePath) -> Result<Metadata> {
        self.metadata(path).await
    }

    async fn lstat(&self, path: &RemotePath) -> Result<Metadata> {
        // FTP has no way to ask about a symlink itself.
        self.metadata(path).await
    }

    async fn read_from(&self, path: &RemotePath, offset: u64) -> Result<ByteStream> {
        let mut lease = self.session().await?;
        let transfer = match lease.retrieve(path, offset).await {
            Ok(transfer) => transfer,
            Err(error) => {
                drop(lease);
                return Err(self.explain(path, error).await);
            }
        };
        let (sender, receiver) = mpsc::channel(4);
        let path = path.clone();
        tokio::spawn(download(lease, transfer, path, sender));
        Ok(tokio_stream(receiver))
    }

    async fn open_writer(&self, path: &RemotePath, offset: u64) -> Result<Box<dyn FileWriter>> {
        if offset == 0 {
            self.ensure_free(path).await?;
        } else {
            let metadata = self.metadata(path).await?;
            if metadata.kind != EntryKind::File || metadata.size < offset {
                return Err(Error::Remote {
                    path: path.clone(),
                    message: "cannot go on writing it there".into(),
                });
            }
        }
        let mut lease = self.session().await?;
        let transfer = match lease.store(path, offset).await {
            Err(error) if offset > 0 && is_refusal(&error) => {
                tracing::debug!(%path, %error, "the server does not go on with uploads");
                self.pool.refuses_resume.store(true, Ordering::Relaxed);
                return Err(Error::Unsupported("going on with an upload"));
            }
            transfer => transfer?,
        };
        Ok(Box::new(FtpWriter {
            transfer: Some(transfer),
            lease: Some(lease),
            remote: self.clone(),
            path: path.clone(),
            offset,
            written: 0,
        }))
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        // Servers replace an existing target; ferge must not.
        self.ensure_free(to).await?;
        let mut lease = self.session().await?;
        let result = lease.rename(from, to).await;
        drop(lease);
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(self.explain(from, error).await),
        }
    }

    async fn rename_replace(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let mut lease = self.session().await?;
        match lease.rename(from, to).await {
            Ok(()) => Ok(()),
            // A server that does not replace: remove the old file first.
            Err(error) if is_refusal(&error) => {
                match self.metadata(to).await {
                    Ok(existing) if existing.kind == EntryKind::File => {}
                    _ => return Err(self.explain(from, error).await),
                }
                lease.delete_file(to).await?;
                lease.rename(from, to).await
            }
            Err(error) => Err(error),
        }
    }

    async fn set_modified(&self, path: &RemotePath, time: SystemTime) -> Result<()> {
        let mut lease = self.session().await?;
        lease
            .set_modified(path, time, self.pool.features.mfmt)
            .await
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<()> {
        let mut lease = self.session().await?;
        let result = lease.delete_file(path).await;
        drop(lease);
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(self.explain(path, error).await),
        }
    }

    async fn delete_dir(&self, path: &RemotePath) -> Result<()> {
        let mut lease = self.session().await?;
        let result = lease.delete_dir(path).await;
        drop(lease);
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(self.explain(path, error).await),
        }
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<()> {
        // ProFTPD answers that it made a folder that was there already.
        self.ensure_free(path).await?;
        let mut lease = self.session().await?;
        let result = lease.mkdir(path).await;
        drop(lease);
        match result {
            Ok(()) => Ok(()),
            Err(error) if is_refusal(&error) => match self.metadata(path).await {
                Ok(_) => Err(Error::AlreadyExists { path: path.clone() }),
                Err(_) => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    async fn chmod(&self, path: &RemotePath, mode: u32) -> Result<()> {
        let mut lease = self.session().await?;
        lease.chmod(path, mode).await
    }
}

/// How many entries the first chunk of a listing has, so the first rows
/// show quickly; later chunks are larger.
const FIRST_CHUNK: usize = 256;
const CHUNK: usize = 4096;

/// Lists `path` into `sender`.
async fn list_into(
    remote: &FtpRemote,
    path: &RemotePath,
    sender: &mpsc::Sender<Result<Vec<crate::entry::Entry>>>,
) -> Result<()> {
    let mut lease = remote.session().await?;
    if let Err(error) = lease.cwd(path).await {
        drop(lease);
        return Err(match remote.metadata(path).await {
            Ok(metadata) if !metadata.is_dir() => Error::Remote {
                path: path.clone(),
                message: "is not a folder".into(),
            },
            Err(Error::NotFound { .. }) => Error::NotFound { path: path.clone() },
            _ => error,
        });
    }
    let format = if remote.pool.features.mlst {
        Format::Mlsd
    } else {
        Format::List
    };
    let transfer = lease.list(path, format).await?;
    let timeout = remote.pool.options.command_timeout;
    let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, transfer);
    let mut line = Vec::with_capacity(256);
    let mut chunk = Vec::with_capacity(FIRST_CHUNK);
    let mut limit = FIRST_CHUNK;
    let mut total = 0_usize;
    loop {
        line.clear();
        let read = match tokio::time::timeout(timeout, reader.read_until(b'\n', &mut line)).await {
            Ok(Ok(read)) => read,
            Ok(Err(_)) => {
                lease.discard();
                return Err(Error::Disconnected);
            }
            Err(_) => {
                lease.discard();
                return Err(Error::Timeout("FTP listing"));
            }
        };
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let Some(entry) = listing::parse_line(format, text) else {
            continue;
        };
        total += 1;
        if total > LISTING_LIMIT {
            lease.discard();
            return Err(too_many_entries(path));
        }
        chunk.push(entry);
        if chunk.len() >= limit {
            limit = CHUNK;
            let full = std::mem::replace(&mut chunk, Vec::with_capacity(CHUNK));
            if sender.send(Ok(full)).await.is_err() {
                // Nobody listens any more; the connection is mid-transfer.
                lease.discard();
                return Ok(());
            }
        }
    }
    lease.finish(reader.into_inner(), Some(path)).await?;
    drop(lease);
    if !chunk.is_empty() {
        let _ = sender.send(Ok(chunk)).await;
    }
    Ok(())
}

/// Reads a download into `sender`, then reads the server's verdict.
async fn download(
    mut lease: Lease,
    mut transfer: Transfer,
    path: RemotePath,
    sender: mpsc::Sender<Result<Bytes>>,
) {
    let timeout = lease.timeout();
    loop {
        let mut buffer = BytesMut::with_capacity(READ_CHUNK);
        while buffer.len() < READ_CHUNK {
            match tokio::time::timeout(timeout, transfer.read_buf(&mut buffer)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => {
                    lease.discard();
                    let _ = sender.send(Err(Error::Disconnected)).await;
                    return;
                }
                Err(_) => {
                    lease.discard();
                    let _ = sender.send(Err(Error::Timeout("FTP download"))).await;
                    return;
                }
            }
        }
        if buffer.is_empty() {
            break;
        }
        let last = buffer.len() < READ_CHUNK;
        if sender.send(Ok(buffer.freeze())).await.is_err() {
            lease.discard();
            return;
        }
        if last {
            break;
        }
    }
    if let Err(error) = lease.finish(transfer, Some(&path)).await {
        let _ = sender.send(Err(error)).await;
    }
}

fn tokio_stream<T: Send + 'static>(
    mut receiver: mpsc::Receiver<T>,
) -> futures::stream::BoxStream<'static, T> {
    futures::stream::poll_fn(move |cx| receiver.poll_recv(cx)).boxed()
}

/// An upload: the data connection of a `STOR`, on a connection of its own.
struct FtpWriter {
    transfer: Option<Transfer>,
    lease: Option<Lease>,
    remote: FtpRemote,
    path: RemotePath,
    offset: u64,
    written: u64,
}

impl Drop for FtpWriter {
    fn drop(&mut self) {
        if self.transfer.is_some() {
            // Abandoned in the middle: the connection is not reused.
            drop(self.transfer.take());
            if let Some(lease) = self.lease.take() {
                lease.discard();
            }
        }
    }
}

#[async_trait]
impl FileWriter for FtpWriter {
    async fn write(&mut self, data: Bytes) -> Result<()> {
        let (Some(transfer), Some(lease)) = (self.transfer.as_mut(), self.lease.as_ref()) else {
            return Err(Error::Disconnected);
        };
        match tokio::time::timeout(lease.timeout(), transfer.write_all(&data)).await {
            Ok(Ok(())) => {
                self.written += data.len() as u64;
                Ok(())
            }
            Ok(Err(_)) => Err(Error::Disconnected),
            Err(_) => Err(Error::Timeout("FTP upload")),
        }
    }

    /// The bytes handed to the connection. Some may still be on the way
    /// when it breaks, but TCP delivers in order: if the file on the server
    /// is at least this long when resuming (which the transfer checks),
    /// everything before is there.
    fn confirmed(&self) -> u64 {
        self.offset + self.written
    }

    async fn finish(mut self: Box<Self>) -> Result<u64> {
        let (Some(transfer), Some(mut lease)) = (self.transfer.take(), self.lease.take()) else {
            return Err(Error::Disconnected);
        };
        lease.finish(transfer, Some(&self.path)).await?;
        let length = self.offset + self.written;
        if self.offset > 0 {
            // Resuming kept the file's start; nothing must remain beyond
            // the new end.
            let size = lease.size(&self.path).await?;
            if size != length {
                return Err(Error::Remote {
                    path: self.path.clone(),
                    message: format!("is {size} bytes long on the server instead of {length}"),
                });
            }
        }
        drop(lease);
        let _ = &self.remote;
        Ok(length)
    }
}

/// The reply to a `SIZE`.
fn parse_size(reply: &str) -> Option<u64> {
    reply.split_whitespace().nth(1)?.parse().ok()
}

/// Whether a reply means "no" to a command because of its argument,
/// rather than that the server cannot do it at all.
fn is_path_refusal(status: Status) -> bool {
    matches!(status.code(), 450 | 451 | 550 | 551 | 552 | 553)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_size_replies() {
        assert_eq!(parse_size("213 34"), Some(34));
        assert_eq!(parse_size("213"), None);
        assert!(is_path_refusal(Status::FileUnavailable));
    }
}
