//! The [`Remote`] abstraction and its backends.
//!
//! A remote is anything that looks like a file system: the local disk, an SFTP
//! server, later FTP, S3 or WebDAV. Every operation is asynchronous and runs on
//! the tokio runtime; nothing here may block the caller's thread.

pub mod local;
pub mod sftp;

use std::fmt;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::io::AsyncRead;

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

/// Writes one file front to back. Implementations may keep several writes in
/// flight; errors of earlier writes surface in a later call or in `finish`.
///
/// Dropping a writer without calling [`finish`](FileWriter::finish) abandons
/// the file: whatever reached the remote stays there.
#[async_trait]
pub trait FileWriter: Send {
    /// Appends `data`.
    async fn write(&mut self, data: Bytes) -> Result<()>;

    /// Bytes from the start of the file that are known to be written without
    /// gaps. Resuming at this offset is always safe, even if the writer was
    /// dropped in the middle of a write.
    fn confirmed(&self) -> u64;

    /// Waits for all outstanding writes and closes the file. Returns the
    /// file's length, i.e. the start offset plus the bytes written.
    async fn finish(self: Box<Self>) -> Result<u64>;
}

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

    /// Whether this is the computer Ferry runs on.
    fn is_local(&self) -> bool {
        false
    }

    /// The directory a session starts in (the login directory for SFTP).
    async fn home_dir(&self) -> Result<RemotePath>;

    /// Resolves symlinks and returns the canonical absolute path.
    async fn canonicalize(&self, path: &RemotePath) -> Result<RemotePath>;

    /// Streams the entries of a directory. `.` and `..` are never included, and
    /// entries whose names are unsafe to use are dropped.
    fn list(&self, path: &RemotePath) -> ListStream;

    /// Metadata of `path`, following symlinks.
    async fn stat(&self, path: &RemotePath) -> Result<Metadata>;

    /// Reads a file from `offset` to its end. Chunks arrive in order.
    async fn read_from(&self, path: &RemotePath, offset: u64) -> Result<ByteStream>;

    /// Reads a whole file.
    async fn read(&self, path: &RemotePath) -> Result<ByteStream> {
        self.read_from(path, 0).await
    }

    /// Opens `path` for writing at `offset`. The file is created if missing;
    /// at offset 0 it is truncated, otherwise its first `offset` bytes are kept
    /// (resuming an interrupted transfer).
    async fn open_writer(&self, path: &RemotePath, offset: u64) -> Result<Box<dyn FileWriter>>;

    /// Creates or truncates `path` and writes `data` to it. Returns the number of
    /// bytes written.
    async fn write(&self, path: &RemotePath, mut data: ByteStream) -> Result<u64> {
        let mut writer = self.open_writer(path, 0).await?;
        while let Some(chunk) = data.next().await {
            writer.write(chunk?).await?;
        }
        writer.finish().await
    }

    /// Renames `from` to `to`. Fails with [`Error::AlreadyExists`] instead of
    /// replacing an existing target.
    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()>;

    /// Renames `from` to `to`, replacing `to` if it exists: atomically where
    /// the protocol supports it.
    async fn rename_replace(&self, from: &RemotePath, to: &RemotePath) -> Result<()>;

    /// Sets the modification (and access) time of a file.
    async fn set_modified(&self, path: &RemotePath, time: SystemTime) -> Result<()>;

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
