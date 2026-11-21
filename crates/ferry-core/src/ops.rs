//! File operations made of several remote calls: deleting folders with their
//! contents, creating empty files, finding free names.

use futures::{StreamExt as _, TryStreamExt as _};

use crate::entry::EntryKind;
use crate::error::{Error, Result};
use crate::path::{EntryName, RemotePath};
use crate::remote::Remote;
use crate::transfer::numbered_name;

/// Deletions kept in flight within one folder; SFTP answers them in parallel.
const PARALLEL_DELETES: usize = 16;

/// Attempts before giving up on finding a free name.
const MAX_NUMBER: u32 = 10_000;

/// Deletes `items`, each with its kind as listed. Folders are deleted with
/// everything in them, deepest first; symbolic links are removed, never
/// followed. `progress` receives the number of objects deleted so far.
///
/// Stops at the first failure; what was deleted until then stays deleted.
pub async fn delete(
    remote: &dyn Remote,
    items: &[(RemotePath, EntryKind)],
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<u64> {
    let mut deleted = 0_u64;
    for (path, kind) in items {
        if *kind == EntryKind::Directory {
            delete_tree(remote, path, &mut deleted, progress).await?;
        } else {
            remote.delete_file(path).await?;
            deleted += 1;
            progress(deleted);
        }
    }
    Ok(deleted)
}

async fn delete_tree(
    remote: &dyn Remote,
    root: &RemotePath,
    deleted: &mut u64,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<()> {
    // Folders are visited twice: first to empty them (pushing subfolders on
    // top), then, once everything above them is gone, to remove them.
    let mut pending = vec![(root.clone(), false)];
    while let Some((dir, emptied)) = pending.pop() {
        if emptied {
            remote.delete_dir(&dir).await?;
            *deleted += 1;
            progress(*deleted);
            continue;
        }
        pending.push((dir.clone(), true));
        let entries: Vec<_> = remote.list(&dir).try_collect::<Vec<_>>().await?;
        let mut files = Vec::new();
        for entry in entries.into_iter().flatten() {
            let path = dir.join(&entry.name);
            if entry.kind() == EntryKind::Directory {
                pending.push((path, false));
            } else {
                files.push(path);
            }
        }
        let mut deletions = futures::stream::iter(files)
            .map(|path| async move { remote.delete_file(&path).await })
            .buffer_unordered(PARALLEL_DELETES);
        while let Some(result) = deletions.next().await {
            result?;
            *deleted += 1;
            progress(*deleted);
        }
    }
    Ok(())
}

/// Creates an empty file; fails if `path` exists.
pub async fn create_file(remote: &dyn Remote, path: &RemotePath) -> Result<()> {
    match remote.stat(path).await {
        Ok(_) => return Err(Error::AlreadyExists { path: path.clone() }),
        Err(Error::NotFound { .. }) => {}
        Err(error) => return Err(error),
    }
    remote.open_writer(path, 0).await?.finish().await.map(drop)
}

/// `base`, or `base (2)`, `base (3)`, … — the first name not taken in `dir`.
pub async fn free_name(remote: &dyn Remote, dir: &RemotePath, base: &str) -> Result<EntryName> {
    for number in 1..MAX_NUMBER {
        let name = if number == 1 {
            EntryName::new(base)?
        } else {
            EntryName::new(&numbered_name(base, number))?
        };
        match remote.stat(&dir.join(&name)).await {
            Err(Error::NotFound { .. }) => return Ok(name),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
    }
    Err(Error::AlreadyExists {
        path: dir.join(&EntryName::new(base)?),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::remote::local::LocalRemote;

    fn path(path: &Path) -> RemotePath {
        RemotePath::parse(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn deletes_trees_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), b"keep").unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("a/b/c")).unwrap();
        for index in 0..40 {
            std::fs::write(tree.join(format!("a/f{index}")), b"x").unwrap();
        }
        std::fs::write(tree.join("a/b/c/deep"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tree.join("a/link")).unwrap();
        std::fs::write(dir.path().join("single"), b"x").unwrap();

        let seen = AtomicU64::new(0);
        let deleted = delete(
            &LocalRemote::new(),
            &[
                (path(&tree), EntryKind::Directory),
                (path(&dir.path().join("single")), EntryKind::File),
            ],
            &|count| seen.store(count, Ordering::Relaxed),
        )
        .await
        .unwrap();
        // 40 files, deep, link, 4 folders, single.
        assert_eq!(deleted, 47);
        assert_eq!(seen.load(Ordering::Relaxed), 47);
        assert!(!tree.exists() && !dir.path().join("single").exists());
        assert!(
            outside.path().join("keep.txt").exists(),
            "links are not followed"
        );
    }

    #[tokio::test]
    async fn creates_files_and_finds_free_names() {
        let dir = tempfile::tempdir().unwrap();
        let remote = LocalRemote::new();
        let root = path(dir.path());
        let name = free_name(&remote, &root, "untitled.txt").await.unwrap();
        assert_eq!(name.as_str(), "untitled.txt");
        create_file(&remote, &root.join(&name)).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("untitled.txt")).unwrap(), b"");
        assert!(matches!(
            create_file(&remote, &root.join(&name)).await,
            Err(Error::AlreadyExists { .. })
        ));
        let next = free_name(&remote, &root, "untitled.txt").await.unwrap();
        assert_eq!(next.as_str(), "untitled (2).txt");
    }
}
