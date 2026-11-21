//! Directory entries and file metadata.

use std::time::{Duration, SystemTime};

use crate::path::EntryName;

/// What kind of object an entry is. Symlinks are reported as such and not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    /// Sockets, devices, FIFOs and anything the server could not classify.
    Other,
}

impl EntryKind {
    const S_IFMT: u32 = 0o170_000;
    const S_IFDIR: u32 = 0o040_000;
    const S_IFREG: u32 = 0o100_000;
    const S_IFLNK: u32 = 0o120_000;

    /// Derives the kind from a POSIX `st_mode` value.
    pub fn from_mode(mode: u32) -> Self {
        match mode & Self::S_IFMT {
            Self::S_IFDIR => Self::Directory,
            Self::S_IFREG => Self::File,
            Self::S_IFLNK => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// Metadata of a single file system object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub kind: EntryKind,
    /// Size in bytes; `0` when the server did not report one.
    pub size: u64,
    /// Last modification time, if known.
    pub modified: Option<SystemTime>,
    /// Permission bits (`0o7777` mask), if known.
    pub permissions: Option<u32>,
}

impl Metadata {
    /// Builds metadata from raw POSIX values as reported by SFTP and `stat(2)`.
    pub fn from_posix(mode: Option<u32>, size: Option<u64>, mtime_secs: Option<u32>) -> Self {
        Self {
            kind: mode.map_or(EntryKind::Other, EntryKind::from_mode),
            size: size.unwrap_or(0),
            modified: mtime_secs
                .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(secs))),
            permissions: mode.map(|mode| mode & 0o7777),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: EntryName,
    pub metadata: Metadata,
}

impl Entry {
    pub fn kind(&self) -> EntryKind {
        self.metadata.kind
    }

    /// Whether opening this entry should navigate into it. Symlinks are included
    /// because their target is only known after asking the server.
    pub fn is_navigable(&self) -> bool {
        matches!(
            self.metadata.kind,
            EntryKind::Directory | EntryKind::Symlink
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_mode() {
        assert_eq!(EntryKind::from_mode(0o040_755), EntryKind::Directory);
        assert_eq!(EntryKind::from_mode(0o100_644), EntryKind::File);
        assert_eq!(EntryKind::from_mode(0o120_777), EntryKind::Symlink);
        assert_eq!(EntryKind::from_mode(0o020_666), EntryKind::Other);
    }

    #[test]
    fn metadata_from_posix() {
        let meta = Metadata::from_posix(Some(0o100_640), Some(42), Some(60));
        assert_eq!(meta.kind, EntryKind::File);
        assert_eq!(meta.size, 42);
        assert_eq!(meta.permissions, Some(0o640));
        assert_eq!(
            meta.modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(60))
        );
        assert_eq!(
            Metadata::from_posix(None, None, None).kind,
            EntryKind::Other
        );
    }
}
