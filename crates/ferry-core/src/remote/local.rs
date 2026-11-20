//! The local file system as a [`Remote`].

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::StreamExt as _;
use tokio::sync::mpsc;

use super::{ByteStream, ListStream, Remote, reader_stream, write_all};
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
    for dir_entry in entries {
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

#[async_trait]
impl Remote for LocalRemote {
    fn label(&self) -> &'static str {
        "Local"
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

    async fn read(&self, path: &RemotePath) -> Result<ByteStream> {
        let file = tokio::fs::File::open(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))?;
        Ok(reader_stream(path.clone(), file))
    }

    async fn write(&self, path: &RemotePath, data: ByteStream) -> Result<u64> {
        let file = tokio::fs::File::create(to_local(path))
            .await
            .map_err(|error| Error::from_io(path, error))?;
        write_all(path, file, data).await
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
}
