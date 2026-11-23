//! The [`Remote`] abstraction and its backends.
//!
//! A remote is anything that looks like a file system: the local disk, an SFTP
//! server, later FTP, S3 or WebDAV. Every operation is asynchronous and runs on
//! the tokio runtime; nothing here may block the caller's thread.

pub mod local;
pub mod offline;
pub mod server;
pub mod sftp;
pub(crate) mod shell;

use std::fmt;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::io::AsyncRead;

use crate::destination::Destination;
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

/// What a backend can do beyond what every one can (list, read, write
/// a new file, rename, delete). The transfer engine and the UI follow it
/// instead of assuming a POSIX file system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent yes/no answers, read one at a time"
)]
pub struct Capabilities {
    /// [`Remote::open_writer`] takes an offset beyond 0, so an interrupted
    /// upload goes on where it stopped. Without it, it starts again.
    pub resume_writes: bool,
    /// A file being written appears under its name only once it is
    /// complete (object stores), so a transfer writes straight to the
    /// destination instead of a `.ferge-part` file, and replaces one
    /// through [`Remote::open_replacing_writer`].
    pub atomic_writes: bool,
    /// [`Remote::set_modified`] works.
    pub set_modified: bool,
    /// [`Remote::chmod`] works and listings report permission bits.
    pub permissions: bool,
}

impl Capabilities {
    /// A POSIX file system: the local disk, SFTP.
    pub const POSIX: Self = Self {
        resume_writes: true,
        atomic_writes: false,
        set_modified: true,
        permissions: true,
    };
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

    /// Whether this is the computer ferge runs on.
    fn is_local(&self) -> bool {
        false
    }

    /// The server behind this remote, if it can copy and run commands on its
    /// own (see [`server`]).
    fn server_side(&self) -> Option<&dyn server::ServerSide> {
        None
    }

    /// What this backend can do.
    fn capabilities(&self) -> Capabilities {
        Capabilities::POSIX
    }

    /// Where to connect to get this file system again (without a start
    /// folder, and as the login user even while working as root). `None`
    /// if it cannot be connected to again.
    fn destination(&self) -> Option<Destination> {
        None
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

    /// Metadata of `path` itself: a symlink is reported as one.
    async fn lstat(&self, path: &RemotePath) -> Result<Metadata>;

    /// Reads a file from `offset` to its end. Chunks arrive in order.
    async fn read_from(&self, path: &RemotePath, offset: u64) -> Result<ByteStream>;

    /// Reads a whole file.
    async fn read(&self, path: &RemotePath) -> Result<ByteStream> {
        self.read_from(path, 0).await
    }

    /// Opens `path` for writing at `offset`.
    ///
    /// At offset 0 a new file is created; anything already at `path`,
    /// including a symlink, makes this fail with [`Error::AlreadyExists`],
    /// so nothing planted there is written through. At a later offset
    /// (resuming an interrupted transfer) `path` must be a regular file,
    /// not a symlink; its first `offset` bytes are kept and the rest is
    /// cut off.
    async fn open_writer(&self, path: &RemotePath, offset: u64) -> Result<Box<dyn FileWriter>>;

    /// Opens `path` for writing from the start, replacing whatever file is
    /// there once the writer finishes. Only backends with
    /// [`Capabilities::atomic_writes`] offer it; until then, the old file
    /// stays as it was.
    async fn open_replacing_writer(&self, path: &RemotePath) -> Result<Box<dyn FileWriter>> {
        let _ = path;
        Err(Error::Unsupported("replacing a file as it is written"))
    }

    /// Creates `path`, which must not exist yet, and writes `data` to it.
    /// Returns the number of bytes written.
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

/// Remotes with operations running on them, by address, with how many.
static IN_USE: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

fn address(remote: &std::sync::Arc<dyn Remote>) -> usize {
    std::sync::Arc::as_ptr(remote).cast::<()>() as usize
}

/// Marks `remote` (this very connection) as in use by an operation such as
/// a delete or a rename, until the guard is dropped, so that it is not
/// closed underneath (see [`crate::transfer::TransferQueue::until_unused`]).
#[derive(Debug)]
pub struct InUse {
    address: usize,
}

impl InUse {
    pub fn new(remote: &std::sync::Arc<dyn Remote>) -> Self {
        let address = address(remote);
        let mut in_use = IN_USE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match in_use.iter_mut().find(|(known, _)| *known == address) {
            Some((_, count)) => *count += 1,
            None => in_use.push((address, 1)),
        }
        Self { address }
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut in_use = IN_USE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = in_use.iter().position(|(known, _)| *known == self.address) {
            in_use[index].1 -= 1;
            if in_use[index].1 == 0 {
                in_use.swap_remove(index);
            }
        }
    }
}

/// Whether an [`InUse`] guard for `remote` is alive.
pub fn is_in_use(remote: &std::sync::Arc<dyn Remote>) -> bool {
    let address = address(remote);
    IN_USE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|(known, _)| *known == address)
}

/// Entries a listing may have at most. A server that never ends a listing
/// (or a folder far beyond what anyone browses) would otherwise fill the
/// memory; past this, the listing fails.
pub const LISTING_LIMIT: usize = 2_000_000;

/// The error for a listing of `path` that went past [`LISTING_LIMIT`].
pub(crate) fn too_many_entries(path: &RemotePath) -> Error {
    Error::Remote {
        path: path.clone(),
        message: "the folder has more than 2,000,000 entries, more than ferge lists".into(),
    }
}

/// Turns an [`AsyncRead`] into a [`ByteStream`], mapping errors to `path`.
pub(crate) fn reader_stream<R>(path: RemotePath, reader: R) -> ByteStream
where
    R: AsyncRead + Send + 'static,
{
    tokio_util::io::ReaderStream::with_capacity(reader, READ_CHUNK)
        .map(move |chunk| chunk.map_err(|error| Error::from_io(&path, error)))
        .boxed()
}

#[cfg(test)]
mod in_use_tests {
    use std::sync::Arc;

    use super::{InUse, Remote, is_in_use};
    use crate::remote::local::LocalRemote;

    #[test]
    fn guards_mark_a_remote_in_use_until_dropped() {
        let remote: Arc<dyn Remote> = Arc::new(LocalRemote::new());
        let other: Arc<dyn Remote> = Arc::new(LocalRemote::new());
        assert!(!is_in_use(&remote));
        let first = InUse::new(&remote);
        let second = InUse::new(&remote);
        assert!(is_in_use(&remote));
        assert!(!is_in_use(&other));
        drop(first);
        assert!(is_in_use(&remote));
        drop(second);
        assert!(!is_in_use(&remote));
    }
}
