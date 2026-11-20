//! The [`Remote`] abstraction and its backends.
//!
//! A remote is anything that looks like a file system: the local disk, an SFTP
//! server, later FTP, S3 or WebDAV. Every operation is asynchronous and runs on
//! the tokio runtime; nothing here may block the caller's thread.

pub mod local;
pub mod sftp;

use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::io::{AsyncRead, AsyncWriteExt as _};

use crate::entry::{Entry, Metadata};
use crate::error::{Error, Result};
use crate::path::RemotePath;

/// A directory listing delivered in chunks, in whatever order the server returns them.
///
/// The stream ends after the last chunk. An `Err` item ends the listing; entries
/// received before it are valid. Dropping the stream cancels the listing.
pub type ListStream = BoxStream<'static, Result<Vec<Entry>>>;

/// File contents delivered in chunks.
pub type ByteStream = BoxStream<'static, Result<Bytes>>;

/// A file system reachable through some protocol.
///
/// Paths are always absolute; use [`Remote::home_dir`] to find a sensible start.
///
/// Methods must be called from within a tokio runtime. The streams they return
/// are fed by tokio tasks and may be polled from any executor.
#[async_trait]
pub trait Remote: Send + Sync + fmt::Debug + 'static {
    /// Short human-readable description such as `user@example.org` or `Local`.
    fn label(&self) -> &str;

    /// The directory a session starts in (the login directory for SFTP).
    async fn home_dir(&self) -> Result<RemotePath>;

    /// Resolves symlinks and returns the canonical absolute path.
    async fn canonicalize(&self, path: &RemotePath) -> Result<RemotePath>;

    /// Streams the entries of a directory. `.` and `..` are never included, and
    /// entries whose names are unsafe to use are dropped.
    fn list(&self, path: &RemotePath) -> ListStream;

    /// Metadata of `path`, following symlinks.
    async fn stat(&self, path: &RemotePath) -> Result<Metadata>;

    /// Opens a file for reading.
    async fn read(&self, path: &RemotePath) -> Result<ByteStream>;

    /// Creates or truncates `path` and writes `data` to it. Returns the number of
    /// bytes written.
    async fn write(&self, path: &RemotePath, data: ByteStream) -> Result<u64>;

    /// Renames `from` to `to`. Fails with [`Error::AlreadyExists`] instead of
    /// replacing an existing target.
    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()>;

    /// Deletes a file or symlink.
    async fn delete_file(&self, path: &RemotePath) -> Result<()>;

    /// Deletes an empty directory.
    async fn delete_dir(&self, path: &RemotePath) -> Result<()>;

    /// Creates a single directory; the parent must exist.
    async fn mkdir(&self, path: &RemotePath) -> Result<()>;

    /// Sets the permission bits (`0o7777` mask) of `path`.
    async fn chmod(&self, path: &RemotePath, mode: u32) -> Result<()>;
}

/// Size of the chunks produced by [`ByteStream`]s of the built-in backends.
pub(crate) const READ_CHUNK: usize = 256 * 1024;

/// Turns an [`AsyncRead`] into a [`ByteStream`], mapping errors to `path`.
pub(crate) fn reader_stream<R>(path: RemotePath, reader: R) -> ByteStream
where
    R: AsyncRead + Send + 'static,
{
    tokio_util::io::ReaderStream::with_capacity(reader, READ_CHUNK)
        .map(move |chunk| chunk.map_err(|error| Error::from_io(&path, error)))
        .boxed()
}

/// Drains `data` into `writer`, then flushes and shuts the writer down.
pub(crate) async fn write_all<W>(
    path: &RemotePath,
    mut writer: W,
    mut data: ByteStream,
) -> Result<u64>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut written = 0_u64;
    while let Some(chunk) = data.next().await {
        let chunk = chunk?;
        writer
            .write_all(&chunk)
            .await
            .map_err(|error| Error::from_io(path, error))?;
        written += chunk.len() as u64;
    }
    writer
        .shutdown()
        .await
        .map_err(|error| Error::from_io(path, error))?;
    Ok(written)
}
