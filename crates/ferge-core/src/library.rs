//! Saved servers ("favorites"), their folders and the connection history,
//! persisted in SQLite.
//!
//! All methods block on disk I/O and must not run on a UI thread. A
//! [`Library`] is `Sync`; calls are serialized by an internal mutex, and
//! several processes may share the same database file.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::destination::Destination;

/// Connections kept in the history; older ones are dropped.
const HISTORY_LIMIT: usize = 50;

/// Maximum length of user-supplied names.
const MAX_NAME_LEN: usize = 200;

/// Schema migrations, applied in order. `PRAGMA user_version` records how many ran.
const MIGRATIONS: &[&str] = &[r"
    CREATE TABLE folders (
        id        INTEGER PRIMARY KEY,
        parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
        name      TEXT NOT NULL
    );
    CREATE TABLE bookmarks (
        id            INTEGER PRIMARY KEY,
        folder_id     INTEGER REFERENCES folders(id) ON DELETE CASCADE,
        name          TEXT NOT NULL,
        address       TEXT NOT NULL,
        identity_file TEXT
    );
    CREATE INDEX bookmarks_folder ON bookmarks(folder_id);
    CREATE TABLE history (
        address      TEXT PRIMARY KEY,
        bookmark_id  INTEGER REFERENCES bookmarks(id) ON DELETE SET NULL,
        connected_at INTEGER NOT NULL,
        -- Orders entries that were recorded within the same second.
        sequence     INTEGER NOT NULL
    );
    CREATE INDEX history_sequence ON history(sequence);
"];

/// Errors of the [`Library`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LibraryError {
    #[error("the library database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("could not create {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The database was written by a newer version of Ferge.
    #[error("the library was created by a newer version of Ferge (schema {0})")]
    TooNew(i64),
    #[error("{0}")]
    Invalid(String),
    #[error("the item no longer exists")]
    NotFound,
}

pub type Result<T, E = LibraryError> = std::result::Result<T, E>;

/// Identifies a [`Folder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId(i64);

/// Identifies a [`Bookmark`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BookmarkId(i64);

/// A folder of bookmarks. Folders nest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub parent: Option<FolderId>,
    pub name: String,
}

/// A saved server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub id: BookmarkId,
    pub folder: Option<FolderId>,
    pub name: String,
    pub destination: Destination,
    /// A private key to try first, in addition to `ssh_config` and the agent.
    pub identity_file: Option<PathBuf>,
}

/// The fields of a bookmark to create or update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookmarkDraft {
    pub folder: Option<FolderId>,
    pub name: String,
    pub destination: Destination,
    pub identity_file: Option<PathBuf>,
}

impl From<Bookmark> for BookmarkDraft {
    fn from(bookmark: Bookmark) -> Self {
        Self {
            folder: bookmark.folder,
            name: bookmark.name,
            destination: bookmark.destination,
            identity_file: bookmark.identity_file,
        }
    }
}

/// A past connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentConnection {
    pub destination: Destination,
    /// The bookmark it was opened from, if that still exists.
    pub bookmark: Option<BookmarkId>,
    pub connected_at: SystemTime,
}

/// Everything the library holds, sorted for display: folders and bookmarks by
/// name, the history newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibrarySnapshot {
    pub folders: Vec<Folder>,
    pub bookmarks: Vec<Bookmark>,
    pub history: Vec<RecentConnection>,
}

impl LibrarySnapshot {
    pub fn bookmark(&self, id: BookmarkId) -> Option<&Bookmark> {
        self.bookmarks.iter().find(|bookmark| bookmark.id == id)
    }

    pub fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.folders.iter().find(|folder| folder.id == id)
    }

    /// Direct subfolders of `parent` (`None` for the top level), by name.
    pub fn subfolders(&self, parent: Option<FolderId>) -> impl Iterator<Item = &Folder> {
        self.folders
            .iter()
            .filter(move |folder| folder.parent == parent)
    }

    /// Bookmarks directly in `folder` (`None` for the top level), by name.
    pub fn bookmarks_in(&self, folder: Option<FolderId>) -> impl Iterator<Item = &Bookmark> {
        self.bookmarks
            .iter()
            .filter(move |bookmark| bookmark.folder == folder)
    }

    /// `folder` followed by all folders nested in it, at any depth.
    pub fn folder_with_descendants(&self, folder: FolderId) -> Vec<FolderId> {
        let mut result = vec![folder];
        let mut index = 0;
        while let Some(&current) = result.get(index) {
            let children: Vec<FolderId> = self
                .subfolders(Some(current))
                .map(|child| child.id)
                .filter(|id| !result.contains(id))
                .collect();
            result.extend(children);
            index += 1;
        }
        result
    }

    /// "Parent / Child" path of a folder, for menus.
    pub fn folder_path(&self, folder: FolderId) -> String {
        let mut names = Vec::new();
        let mut current = self.folder(folder);
        while let Some(found) = current {
            if names.len() > self.folders.len() {
                break; // a cycle; cannot happen through this API
            }
            names.push(found.name.as_str());
            current = found.parent.and_then(|parent| self.folder(parent));
        }
        names.reverse();
        names.join(" / ")
    }
}

/// The persistent store of bookmarks, folders and history.
#[derive(Debug)]
pub struct Library {
    connection: Mutex<Connection>,
}

impl Library {
    /// `ferge/library.sqlite3` in the platform's data directory
    /// (`~/.local/share` on Linux, `~/Library/Application Support` on macOS).
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|dir| dir.join("ferge").join("library.sqlite3"))
    }

    /// Opens or creates the database at `path` and brings its schema up to date.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Self::with_connection(connection)
    }

    /// A library that lives in memory only, for tests and benchmarks.
    pub fn open_in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(mut connection: Connection) -> Result<Self> {
        connection.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite inconsistent:
        // every multi-statement change runs in a transaction.
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads everything at once.
    pub fn snapshot(&self) -> Result<LibrarySnapshot> {
        let connection = self.connection();
        let mut folders = connection
            .prepare("SELECT id, parent_id, name FROM folders")?
            .query_map([], |row| {
                Ok(Folder {
                    id: FolderId(row.get(0)?),
                    parent: row.get::<_, Option<i64>>(1)?.map(FolderId),
                    name: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        folders.sort_by(|a, b| compare_names(&a.name, &b.name));

        let mut bookmarks = Vec::new();
        let mut statement = connection
            .prepare("SELECT id, folder_id, name, address, identity_file FROM bookmarks")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, folder, name, address, identity_file) = row?;
            match Destination::parse(&address) {
                Ok(destination) => bookmarks.push(Bookmark {
                    id: BookmarkId(id),
                    folder: folder.map(FolderId),
                    name,
                    destination,
                    identity_file: identity_file.map(PathBuf::from),
                }),
                Err(error) => tracing::warn!(id, %error, "skipping a bookmark with a bad address"),
            }
        }
        bookmarks.sort_by(|a, b| compare_names(&a.name, &b.name));

        let mut history = Vec::new();
        let mut statement = connection.prepare(
            "SELECT address, bookmark_id, connected_at FROM history ORDER BY sequence DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (address, bookmark, connected_at) = row?;
            if let Ok(destination) = Destination::parse(&address) {
                history.push(RecentConnection {
                    destination,
                    bookmark: bookmark.map(BookmarkId),
                    connected_at: from_unix(connected_at),
                });
            }
        }
        Ok(LibrarySnapshot {
            folders,
            bookmarks,
            history,
        })
    }

    /// Creates a folder in `parent` (`None` for the top level).
    pub fn create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId> {
        let name = validate_name(name)?;
        let connection = self.connection();
        if let Some(parent) = parent {
            ensure_folder_exists(&connection, parent)?;
        }
        connection.execute(
            "INSERT INTO folders (parent_id, name) VALUES (?1, ?2)",
            params![parent.map(|id| id.0), name],
        )?;
        Ok(FolderId(connection.last_insert_rowid()))
    }

    pub fn rename_folder(&self, folder: FolderId, name: &str) -> Result<()> {
        let name = validate_name(name)?;
        let changed = self.connection().execute(
            "UPDATE folders SET name = ?2 WHERE id = ?1",
            params![folder.0, name],
        )?;
        expect_change(changed)
    }

    /// Moves `folder` into `parent`. A folder cannot move into itself or one
    /// of its descendants.
    pub fn move_folder(&self, folder: FolderId, parent: Option<FolderId>) -> Result<()> {
        let mut connection = self.connection();
        let transaction = connection.transaction()?;
        if let Some(parent) = parent {
            ensure_folder_exists(&transaction, parent)?;
            // Walk up from the new parent; meeting `folder` means a cycle.
            let mut current = Some(parent);
            while let Some(id) = current {
                if id == folder {
                    return Err(LibraryError::Invalid(
                        "a folder cannot be moved into itself".into(),
                    ));
                }
                current = transaction
                    .query_row(
                        "SELECT parent_id FROM folders WHERE id = ?1",
                        [id.0],
                        |row| row.get::<_, Option<i64>>(0),
                    )?
                    .map(FolderId);
            }
        }
        let changed = transaction.execute(
            "UPDATE folders SET parent_id = ?2 WHERE id = ?1",
            params![folder.0, parent.map(|id| id.0)],
        )?;
        expect_change(changed)?;
        transaction.commit()?;
        Ok(())
    }

    /// Deletes `folder` with everything in it.
    pub fn delete_folder(&self, folder: FolderId) -> Result<()> {
        let changed = self
            .connection()
            .execute("DELETE FROM folders WHERE id = ?1", [folder.0])?;
        expect_change(changed)
    }

    /// Saves a new bookmark.
    pub fn create_bookmark(&self, draft: &BookmarkDraft) -> Result<BookmarkId> {
        let name = validate_name(&draft.name)?;
        let connection = self.connection();
        if let Some(folder) = draft.folder {
            ensure_folder_exists(&connection, folder)?;
        }
        connection.execute(
            "INSERT INTO bookmarks (folder_id, name, address, identity_file) VALUES (?1, ?2, ?3, ?4)",
            params![
                draft.folder.map(|id| id.0),
                name,
                draft.destination.address(),
                identity_file_text(draft)?,
            ],
        )?;
        Ok(BookmarkId(connection.last_insert_rowid()))
    }

    /// Replaces the fields of an existing bookmark.
    pub fn update_bookmark(&self, bookmark: BookmarkId, draft: &BookmarkDraft) -> Result<()> {
        let name = validate_name(&draft.name)?;
        let connection = self.connection();
        if let Some(folder) = draft.folder {
            ensure_folder_exists(&connection, folder)?;
        }
        let changed = connection.execute(
            "UPDATE bookmarks SET folder_id = ?2, name = ?3, address = ?4, identity_file = ?5
             WHERE id = ?1",
            params![
                bookmark.0,
                draft.folder.map(|id| id.0),
                name,
                draft.destination.address(),
                identity_file_text(draft)?,
            ],
        )?;
        expect_change(changed)
    }

    pub fn delete_bookmark(&self, bookmark: BookmarkId) -> Result<()> {
        let changed = self
            .connection()
            .execute("DELETE FROM bookmarks WHERE id = ?1", [bookmark.0])?;
        expect_change(changed)
    }

    /// Records a successful connection. Repeated connections to the same
    /// address update the existing entry.
    pub fn record_connection(
        &self,
        destination: &Destination,
        bookmark: Option<BookmarkId>,
    ) -> Result<()> {
        let mut connection = self.connection();
        let transaction = connection.transaction()?;
        // The bookmark may have been deleted meanwhile; do not fail for that.
        let bookmark = match bookmark {
            Some(id) => transaction
                .query_row("SELECT id FROM bookmarks WHERE id = ?1", [id.0], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?,
            None => None,
        };
        transaction.execute(
            "INSERT INTO history (address, bookmark_id, connected_at, sequence)
             VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(sequence), 0) + 1 FROM history))
             ON CONFLICT(address) DO UPDATE SET
                 bookmark_id = excluded.bookmark_id,
                 connected_at = excluded.connected_at,
                 sequence = excluded.sequence",
            params![destination.address(), bookmark, to_unix(SystemTime::now())],
        )?;
        transaction.execute(
            "DELETE FROM history WHERE address NOT IN
                 (SELECT address FROM history ORDER BY sequence DESC LIMIT ?1)",
            [i64::try_from(HISTORY_LIMIT).unwrap_or(i64::MAX)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes one entry from the history.
    pub fn forget_connection(&self, destination: &Destination) -> Result<()> {
        self.connection().execute(
            "DELETE FROM history WHERE address = ?1",
            [destination.address()],
        )?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<()> {
        self.connection().execute("DELETE FROM history", [])?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if version > known {
        return Err(LibraryError::TooNew(version));
    }
    let start = usize::try_from(version).unwrap_or(0);
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(start) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.pragma_update(
            None,
            "user_version",
            i64::try_from(index + 1).unwrap_or(i64::MAX),
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn ensure_folder_exists(connection: &Connection, folder: FolderId) -> Result<()> {
    connection
        .query_row(
            "SELECT 1 FROM folders WHERE id = ?1",
            [folder.0],
            |_| Ok(()),
        )
        .optional()?
        .ok_or(LibraryError::NotFound)
}

fn expect_change(changed: usize) -> Result<()> {
    if changed == 0 {
        Err(LibraryError::NotFound)
    } else {
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LibraryError::Invalid("the name must not be empty".into()));
    }
    if name.chars().count() > MAX_NAME_LEN || name.chars().any(char::is_control) {
        return Err(LibraryError::Invalid(format!(
            "names are limited to {MAX_NAME_LEN} printable characters"
        )));
    }
    Ok(name)
}

fn identity_file_text(draft: &BookmarkDraft) -> Result<Option<&str>> {
    draft
        .identity_file
        .as_deref()
        .map(|path| {
            path.to_str().ok_or_else(|| {
                LibraryError::Invalid("the key file path must be valid UTF-8".into())
            })
        })
        .transpose()
}

/// Case-insensitive order with a case-sensitive tie-breaker, so the order is total.
fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

fn to_unix(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
    })
}

fn from_unix(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
}

/// Creates `dir` and its parents; on Unix the last component is only
/// accessible by the owner, since the library reveals which servers the user
/// works with.
fn create_private_dir(dir: &Path) -> Result<()> {
    let error = |source| LibraryError::CreateDir {
        path: dir.to_path_buf(),
        source,
    };
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::SftpTarget;

    fn sftp(host: &str) -> Destination {
        Destination::Sftp(SftpTarget::new(host).unwrap())
    }

    fn draft(name: &str, folder: Option<FolderId>, host: &str) -> BookmarkDraft {
        BookmarkDraft {
            folder,
            name: name.into(),
            destination: sftp(host),
            identity_file: None,
        }
    }

    #[test]
    fn bookmarks_and_folders_round_trip() {
        let library = Library::open_in_memory().unwrap();
        let work = library.create_folder(None, " Work ").unwrap();
        let web = library.create_folder(Some(work), "Web").unwrap();
        let mut full = draft("prod", Some(web), "prod.example.org");
        full.identity_file = Some("/home/me/.ssh/id_prod".into());
        if let Destination::Sftp(target) = &mut full.destination {
            target.user = Some("deploy".into());
            target.port = Some(2222);
            target.path = Some(crate::RemotePath::parse("/srv/www").unwrap());
        }
        let prod = library.create_bookmark(&full).unwrap();
        library.create_bookmark(&draft("alpha", None, "a")).unwrap();
        library.create_bookmark(&draft("Beta", None, "b")).unwrap();

        let snapshot = library.snapshot().unwrap();
        assert_eq!(snapshot.folder(work).unwrap().name, "Work");
        assert_eq!(snapshot.folder_path(web), "Work / Web");
        let stored = snapshot.bookmark(prod).unwrap();
        assert_eq!(BookmarkDraft::from(stored.clone()), full);
        let top: Vec<_> = snapshot
            .bookmarks_in(None)
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(top, ["alpha", "Beta"]);
        assert_eq!(snapshot.folder_with_descendants(work), [work, web]);

        let mut renamed = full.clone();
        renamed.name = "production".into();
        renamed.folder = None;
        library.update_bookmark(prod, &renamed).unwrap();
        assert_eq!(
            library.snapshot().unwrap().bookmark(prod).unwrap().name,
            "production"
        );
    }

    #[test]
    fn deleting_a_folder_deletes_its_contents() {
        let library = Library::open_in_memory().unwrap();
        let outer = library.create_folder(None, "outer").unwrap();
        let inner = library.create_folder(Some(outer), "inner").unwrap();
        let bookmark = library
            .create_bookmark(&draft("x", Some(inner), "x"))
            .unwrap();
        library
            .record_connection(&sftp("x"), Some(bookmark))
            .unwrap();
        library.delete_folder(outer).unwrap();
        let snapshot = library.snapshot().unwrap();
        assert!(snapshot.folders.is_empty() && snapshot.bookmarks.is_empty());
        // The history survives without its bookmark.
        assert_eq!(snapshot.history.len(), 1);
        assert_eq!(snapshot.history[0].bookmark, None);
    }

    #[test]
    fn folders_cannot_form_cycles() {
        let library = Library::open_in_memory().unwrap();
        let a = library.create_folder(None, "a").unwrap();
        let b = library.create_folder(Some(a), "b").unwrap();
        let c = library.create_folder(Some(b), "c").unwrap();
        assert!(matches!(
            library.move_folder(a, Some(c)),
            Err(LibraryError::Invalid(_))
        ));
        assert!(library.move_folder(a, Some(a)).is_err());
        library.move_folder(c, None).unwrap();
        library.move_folder(a, Some(c)).unwrap();
        assert_eq!(library.snapshot().unwrap().folder_path(b), "c / a / b");
    }

    #[test]
    fn rejects_bad_input() {
        let library = Library::open_in_memory().unwrap();
        assert!(library.create_folder(None, "  ").is_err());
        assert!(library.create_folder(None, "a\nb").is_err());
        assert!(library.create_folder(Some(FolderId(42)), "x").is_err());
        assert!(matches!(
            library.rename_folder(FolderId(42), "x"),
            Err(LibraryError::NotFound)
        ));
        assert!(matches!(
            library.delete_bookmark(BookmarkId(7)),
            Err(LibraryError::NotFound)
        ));
        let long = "x".repeat(MAX_NAME_LEN + 1);
        assert!(library.create_bookmark(&draft(&long, None, "h")).is_err());
    }

    #[test]
    fn history_is_deduplicated_and_bounded() {
        let library = Library::open_in_memory().unwrap();
        for i in 0..HISTORY_LIMIT + 10 {
            library
                .record_connection(&sftp(&format!("host{i}")), None)
                .unwrap();
        }
        library.record_connection(&sftp("host0"), None).unwrap();
        let history = library.snapshot().unwrap().history;
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].destination, sftp("host0"));
        assert_eq!(
            history[1].destination,
            sftp(&format!("host{}", HISTORY_LIMIT + 9))
        );
        library.forget_connection(&sftp("host0")).unwrap();
        assert_eq!(library.snapshot().unwrap().history.len(), HISTORY_LIMIT - 1);
        library.clear_history().unwrap();
        assert_eq!(library.snapshot().unwrap().history, []);
    }

    #[test]
    fn persists_and_migrates_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/library.sqlite3");
        {
            let library = Library::open(&path).unwrap();
            library.create_bookmark(&draft("kept", None, "h")).unwrap();
        }
        let library = Library::open(&path).unwrap();
        assert_eq!(library.snapshot().unwrap().bookmarks[0].name, "kept");
        drop(library);

        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(matches!(
            Library::open(&path),
            Err(LibraryError::TooNew(99))
        ));
    }
}
