//! The local file system as a [`Remote`].

use std::fs;
use std::path::{Path, PathBuf};

use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt as _;
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::{ByteStream, FileWriter, ListStream, Remote, reader_stream};
use crate::entry::{Entry, EntryKind, Metadata};
use crate::error::{Error, Result};
use crate::path::{EntryName, RemotePath};

/// Number of entries per listing chunk. Large enough to amortize channel overhead,
/// small enough that the first rows show up immediately.
const LIST_CHUNK: usize = 1024;

/// The local file system, addressed with absolute Unix paths.
#[derive(Debug, Default, Clone)]
pub struct LocalRemote {
    _private: (),
}

impl LocalRemote {
    pub fn new() -> Self {
        Self::default()
    }
}

fn to_local(path: &RemotePath) -> PathBuf {
    PathBuf::from(path.as_str())
}

fn to_remote(path: &Path) -> Result<RemotePath> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::InvalidPath(path.to_string_lossy().into()))?;
    RemotePath::parse(text)
}

/// Opens the regular file at `local` for writing, refusing a symlink or
/// anything else: the file opened must be the one `lstat` saw, so a link
/// swapped in between is noticed too.
async fn open_existing_file(path: &RemotePath, local: &std::path::Path) -> Result<tokio::fs::File> {
    let io = |error| Error::from_io(path, error);
    let seen = tokio::fs::symlink_metadata(local).await.map_err(io)?;
    if !seen.file_type().is_file() {
        return Err(Error::Remote {
            path: path.clone(),
            message: "not a regular file".into(),
        });
    }
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(local)
        .await
        .map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let opened = file.metadata().await.map_err(io)?;
        if (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
            return Err(Error::Remote {
                path: path.clone(),
                message: "the file was replaced while it was opened".into(),
            });
        }
    }
    Ok(file)
}

fn metadata_from_std(metadata: &fs::Metadata) -> Metadata {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Metadata {
            kind: EntryKind::from_mode(metadata.mode()),
            size: metadata.len(),
            modified: metadata.modified().ok(),
            permissions: Some(metadata.mode() & 0o7777),
        }
    }
    #[cfg(not(unix))]
    {
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        Metadata {
            kind,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            permissions: None,
        }
    }
}

/// Reads a directory on a blocking thread and sends it in chunks.
///
/// Returns early when the receiver is dropped, which cancels the listing.
fn list_blocking(path: &RemotePath, sender: &mpsc::Sender<Result<Vec<Entry>>>) {
    let entries = match fs::read_dir(to_local(path)) {
        Ok(entries) => entries,
        Err(error) => {
            let _ = sender.blocking_send(Err(Error::from_io(path, error)));
            return;
        }
    };
    let mut chunk = Vec::with_capacity(LIST_CHUNK);
    for (index, dir_entry) in entries.enumerate() {
        if index == super::LISTING_LIMIT {
            let _ = sender.blocking_send(Err(super::too_many_entries(path)));
            return;
        }
        let dir_entry = match dir_entry {
            Ok(dir_entry) => dir_entry,
            Err(error) => {
                let _ = sender.blocking_send(Err(Error::from_io(path, error)));
                return;
            }
        };
        let file_name = dir_entry.file_name();
        let Some(name) = file_name
            .to_str()
            .and_then(|name| EntryName::new(name).ok())
        else {
            tracing::warn!(dir = %path, name = ?file_name, "skipping entry with a non-UTF-8 name");
            continue;
        };
        // `DirEntry::metadata` does not follow symlinks. An entry that vanished in
        // the meantime is simply skipped.
        let Ok(metadata) = dir_entry.metadata() else {
            continue;
        };
        chunk.push(Entry {
            name,
            metadata: metadata_from_std(&metadata),
        });
        if chunk.len() == LIST_CHUNK {
            let full = std::mem::replace(&mut chunk, Vec::with_capacity(LIST_CHUNK));
            if sender.blocking_send(Ok(full)).is_err() {
                return;
            }
        }
    }
    if !chunk.is_empty() {
        let _ = sender.blocking_send(Ok(chunk));
    }
}

/// Sequential writes to a local file. Chunks are large, so there is no extra
/// buffering; every write is flushed, which keeps [`FileWriter::confirmed`]
/// exact.
struct LocalWriter {
    path: RemotePath,
    file: tokio::fs::File,
    length: u64,
}

#[async_trait]
impl FileWriter for LocalWriter {
    async fn write(&mut self, data: Bytes) -> Result<()> {
        let io = |error| Error::from_io(&self.path, error);
        self.file.write_all(&data).await.map_err(io)?;
        // tokio hands writes to a background thread; wait until it is done.
        self.file.flush().await.map_err(io)?;
        self.length += data.len() as u64;
        Ok(())
    }

    fn confirmed(&self) -> u64 {
        self.length
    }

    async fn finish(mut self: Box<Self>) -> Result<u64> {
        self.file
            .shutdown()
            .await
            .map_err(|error| Error::from_io(&self.path, error))?;
        Ok(self.length)
    }
}

#[async_trait]
impl Remote for LocalRemote {
    fn label(&self) -> &'static str {
        "Local"
    }

    fn is_local(&self) -> bool {
        true
    }

    fn destination(&self) -> Option<crate::Destination> {
        Some(crate::Destination::Local { path: None })
    }

    async fn home_dir(&self) -> Result<RemotePath> {
        let home = dirs::home_dir().ok_or_else(|| Error::Config("no home directory".into()))?;
        to_remote(&home)
    }

    async fn canonicalize(&self, path: &RemotePath) -> Result<RemotePath> {
        let canonical = tokio::fs::canonicalize(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))?;
        to_remote(&canonical)
    }

    fn list(&self, path: &RemotePath) -> ListStream {
        let (sender, receiver) = mpsc::channel(4);
        let path = path.clone();
        tokio::task::spawn_blocking(move || list_blocking(&path, &sender));
        futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        })
        .boxed()
    }

    async fn stat(&self, path: &RemotePath) -> Result<Metadata> {
        let metadata = tokio::fs::metadata(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))?;
        Ok(metadata_from_std(&metadata))
    }

    async fn lstat(&self, path: &RemotePath) -> Result<Metadata> {
        let metadata = tokio::fs::symlink_metadata(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))?;
        Ok(metadata_from_std(&metadata))
    }

    async fn read_from(&self, path: &RemotePath, offset: u64) -> Result<ByteStream> {
        let mut file = tokio::fs::File::open(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))?;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|error| Error::from_io(path, error))?;
        }
        Ok(reader_stream(path.clone(), file))
    }

    async fn open_writer(&self, path: &RemotePath, offset: u64) -> Result<Box<dyn FileWriter>> {
        let io = |error| Error::from_io(path, error);
        let local = to_local(path);
        let mut file = if offset == 0 {
            // `create_new` (O_EXCL) also refuses a symlink, dangling or not.
            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&local)
                .await
                .map_err(io)?
        } else {
            open_existing_file(path, &local).await?
        };
        if offset > 0 {
            // Drop anything past the resume point, e.g. a partly written chunk.
            file.set_len(offset).await.map_err(io)?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(io)?;
        }
        Ok(Box::new(LocalWriter {
            path: path.clone(),
            file,
            length: offset,
        }))
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        // `rename(2)` silently replaces the target. The check below keeps the
        // contract of `Remote::rename`; a concurrent creation of `to` by another
        // process between check and rename is not detected.
        match tokio::fs::symlink_metadata(to_local(to)).await {
            Ok(_) => return Err(Error::AlreadyExists { path: to.clone() }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::from_io(to, error)),
        }
        tokio::fs::rename(to_local(from), to_local(to))
            .await
            .map_err(|error| Error::from_io(from, error))
    }

    async fn rename_replace(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        // `rename(2)` replaces the target atomically.
        tokio::fs::rename(to_local(from), to_local(to))
            .await
            .map_err(|error| Error::from_io(from, error))
    }

    async fn set_modified(&self, path: &RemotePath, time: SystemTime) -> Result<()> {
        let local = to_local(path);
        let result = tokio::task::spawn_blocking(move || {
            let file = fs::File::options().write(true).open(local)?;
            file.set_times(fs::FileTimes::new().set_modified(time).set_accessed(time))
        })
        .await
        .map_err(|error| Error::from_io(path, std::io::Error::other(error)))?;
        result.map_err(|error| Error::from_io(path, error))
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<()> {
        tokio::fs::remove_file(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))
    }

    async fn delete_dir(&self, path: &RemotePath) -> Result<()> {
        tokio::fs::remove_dir(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<()> {
        tokio::fs::create_dir(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))
    }

    async fn chmod(&self, path: &RemotePath, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let permissions = fs::Permissions::from_mode(mode & 0o7777);
            tokio::fs::set_permissions(to_local(path), permissions)
                .await
                .map_err(|error| Error::from_io(path, error))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Err(Error::Unsupported("chmod on this platform"))
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::TryStreamExt as _;

    use super::*;

    fn remote_path(path: &Path) -> RemotePath {
        to_remote(path).expect("temp dir path is valid UTF-8")
    }

    async fn collect(remote: &LocalRemote, path: &RemotePath) -> Vec<Entry> {
        let chunks: Vec<Vec<Entry>> = remote.list(path).try_collect().await.expect("listing");
        let mut entries: Vec<Entry> = chunks.into_iter().flatten().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    #[tokio::test]
    async fn lists_in_chunks_with_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..(LIST_CHUNK + 10) {
            fs::write(dir.path().join(format!("f{i:05}")), b"abc").expect("write");
        }
        fs::create_dir(dir.path().join("sub")).expect("mkdir");

        let remote = LocalRemote::new();
        let root = remote_path(dir.path());
        let chunks: Vec<Vec<Entry>> = remote.list(&root).try_collect().await.expect("listing");
        assert!(chunks.len() >= 2, "listing must be chunked");

        let entries = collect(&remote, &root).await;
        assert_eq!(entries.len(), LIST_CHUNK + 11);
        let first = &entries[0];
        assert_eq!(first.name.as_str(), "f00000");
        assert_eq!(first.kind(), EntryKind::File);
        assert_eq!(first.metadata.size, 3);
        assert!(first.metadata.modified.is_some());
        let sub = entries.last().expect("sub");
        assert_eq!(sub.name.as_str(), "sub");
        assert!(sub.metadata.is_dir());
    }

    #[tokio::test]
    async fn listing_missing_directory_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = remote_path(&dir.path().join("missing"));
        let result: Result<Vec<Vec<Entry>>> = LocalRemote::new().list(&missing).try_collect().await;
        assert!(matches!(result, Err(Error::NotFound { .. })));
    }

    #[tokio::test]
    async fn file_operations_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = LocalRemote::new();
        let root = remote_path(dir.path());
        let name = |n: &str| EntryName::new(n).expect("valid name");
        let file = root.join(&name("a.txt"));
        let renamed = root.join(&name("b.txt"));
        let subdir = root.join(&name("d"));

        let data = futures::stream::iter([
            Ok(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"world")),
        ])
        .boxed();
        assert_eq!(remote.write(&file, data).await.expect("write"), 11);
        let read: Vec<Bytes> = remote
            .read(&file)
            .await
            .expect("open")
            .try_collect()
            .await
            .expect("read");
        assert_eq!(read.concat(), b"hello world");

        remote.chmod(&file, 0o600).await.expect("chmod");
        assert_eq!(
            remote.stat(&file).await.expect("stat").permissions,
            Some(0o600)
        );

        remote.mkdir(&subdir).await.expect("mkdir");
        assert!(matches!(
            remote.rename(&file, &subdir).await,
            Err(Error::AlreadyExists { .. })
        ));
        remote.rename(&file, &renamed).await.expect("rename");
        assert!(matches!(
            remote.stat(&file).await,
            Err(Error::NotFound { .. })
        ));

        remote.delete_file(&renamed).await.expect("delete file");
        remote.delete_dir(&subdir).await.expect("delete dir");
        assert_eq!(collect(&remote, &root).await, Vec::new());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writers_never_follow_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = LocalRemote::new();
        let root = remote_path(dir.path());
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"precious").expect("write");
        let link = root.join(&EntryName::new("link").expect("name"));
        std::os::unix::fs::symlink(&victim, link.as_str()).expect("symlink");
        let dangling = root.join(&EntryName::new("dangling").expect("name"));
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dangling.as_str()).expect("symlink");

        assert!(matches!(
            remote.open_writer(&link, 0).await.map(drop),
            Err(Error::AlreadyExists { .. })
        ));
        assert!(remote.open_writer(&dangling, 0).await.is_err());
        assert!(!dir.path().join("nowhere").exists());
        assert!(remote.open_writer(&link, 3).await.is_err());
        assert_eq!(std::fs::read(&victim).expect("read"), b"precious");
        assert_eq!(
            remote.lstat(&link).await.expect("lstat").kind,
            EntryKind::Symlink
        );
    }

    #[tokio::test]
    async fn resumes_and_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = LocalRemote::new();
        let root = remote_path(dir.path());
        let file = root.join(&EntryName::new("f").expect("name"));
        let other = root.join(&EntryName::new("g").expect("name"));

        let mut writer = remote.open_writer(&file, 0).await.expect("open");
        writer
            .write(Bytes::from_static(b"hello world"))
            .await
            .expect("write");
        assert_eq!(writer.finish().await.expect("finish"), 11);
        // Resuming at 5 drops the rest and continues from there.
        let mut writer = remote.open_writer(&file, 5).await.expect("reopen");
        writer
            .write(Bytes::from_static(b", you"))
            .await
            .expect("write");
        assert_eq!(writer.finish().await.expect("finish"), 10);
        assert_eq!(fs::read(dir.path().join("f")).expect("read"), b"hello, you");

        let tail: Vec<Bytes> = remote
            .read_from(&file, 7)
            .await
            .expect("open")
            .try_collect()
            .await
            .expect("read");
        assert_eq!(tail.concat(), b"you");

        fs::write(dir.path().join("g"), b"old").expect("write");
        remote.rename_replace(&file, &other).await.expect("replace");
        assert_eq!(fs::read(dir.path().join("g")).expect("read"), b"hello, you");
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        remote.set_modified(&other, when).await.expect("mtime");
        assert_eq!(
            remote.stat(&other).await.expect("stat").modified,
            Some(when)
        );
        assert!(remote.is_local());
    }
}
