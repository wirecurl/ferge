//! Protocol-independent remote paths and validated entry names.
//!
//! Remote file systems are addressed with absolute, POSIX-style paths. They are kept
//! separate from [`std::path::Path`] on purpose: a remote path never depends on the
//! local platform, and every path that leaves this module is normalized.

use std::fmt;
use std::sync::Arc;

use crate::error::Error;

/// An absolute, normalized path on a remote (or local) file system.
///
/// Invariants, upheld by every constructor:
/// - starts with `/`,
/// - contains no empty, `.` or `..` components,
/// - has no trailing slash (except for the root itself),
/// - contains no NUL byte.
///
/// Cloning is cheap: the string is reference counted.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath(Arc<str>);

impl RemotePath {
    /// The root directory `/`.
    pub fn root() -> Self {
        Self(Arc::from("/"))
    }

    /// Parses an absolute path and normalizes it lexically.
    ///
    /// Duplicate slashes and `.` components are removed, `..` steps up one level
    /// (never above the root). Relative paths and paths containing NUL are rejected.
    pub fn parse(path: &str) -> Result<Self, Error> {
        if !path.starts_with('/') || path.contains('\0') {
            return Err(Error::InvalidPath(path.into()));
        }
        let mut components: Vec<&str> = Vec::new();
        for component in path.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    components.pop();
                }
                name => components.push(name),
            }
        }
        if components.is_empty() {
            return Ok(Self::root());
        }
        let capacity = components.iter().map(|c| c.len() + 1).sum();
        let mut normalized = String::with_capacity(capacity);
        for component in components {
            normalized.push('/');
            normalized.push_str(component);
        }
        Ok(Self(Arc::from(normalized)))
    }

    /// The path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the root directory.
    pub fn is_root(&self) -> bool {
        &*self.0 == "/"
    }

    /// The parent directory, or `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let cut = self.0.rfind('/').unwrap_or(0);
        if cut == 0 {
            Some(Self::root())
        } else {
            Some(Self(Arc::from(&self.0[..cut])))
        }
    }

    /// The last component, or `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    /// Appends a single, already validated entry name.
    #[must_use]
    pub fn join(&self, name: &EntryName) -> Self {
        let mut joined = String::with_capacity(self.0.len() + 1 + name.as_str().len());
        joined.push_str(&self.0);
        if !self.is_root() {
            joined.push('/');
        }
        joined.push_str(name.as_str());
        Self(Arc::from(joined))
    }

    /// Iterates over the path from the root down to `self`, inclusive.
    ///
    /// `/a/b` yields `/`, `/a`, `/a/b`. Useful for breadcrumbs.
    pub fn ancestors_from_root(&self) -> Vec<Self> {
        let mut chain: Vec<Self> =
            std::iter::successors(Some(self.clone()), Self::parent).collect();
        chain.reverse();
        chain
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl AsRef<str> for RemotePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A single directory entry name that is safe to join onto a path.
///
/// Servers are untrusted: a name received from a listing must never be able to
/// escape its directory. An `EntryName` is guaranteed to be non-empty, to differ
/// from `.` and `..`, and to contain neither `/` nor NUL.
///
/// Cloning is cheap: the string is reference counted.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryName(Arc<str>);

impl EntryName {
    /// Validates a raw name.
    pub fn new(name: &str) -> Result<Self, Error> {
        if Self::is_valid(name) {
            Ok(Self(Arc::from(name)))
        } else {
            Err(Error::InvalidName(name.into()))
        }
    }

    /// Whether `name` would be accepted by [`EntryName::new`].
    pub fn is_valid(name: &str) -> bool {
        !(name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the name starts with a dot (hidden by convention on Unix).
    pub fn is_hidden(&self) -> bool {
        self.0.starts_with('.')
    }
}

impl fmt::Display for EntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for EntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl AsRef<str> for EntryName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parse_normalizes() {
        assert_eq!(path("/").as_str(), "/");
        assert_eq!(path("//").as_str(), "/");
        assert_eq!(path("/a//b/./c/").as_str(), "/a/b/c");
        assert_eq!(path("/a/b/../c").as_str(), "/a/c");
        assert_eq!(path("/../..").as_str(), "/");
    }

    #[test]
    fn parse_rejects_relative_and_nul() {
        assert!(RemotePath::parse("").is_err());
        assert!(RemotePath::parse("a/b").is_err());
        assert!(RemotePath::parse("/a\0b").is_err());
    }

    #[test]
    fn parent_and_file_name() {
        assert_eq!(path("/").parent(), None);
        assert_eq!(path("/a").parent(), Some(path("/")));
        assert_eq!(path("/a/b").parent(), Some(path("/a")));
        assert_eq!(path("/").file_name(), None);
        assert_eq!(path("/a/b").file_name(), Some("b"));
    }

    #[test]
    fn join_and_ancestors() {
        let name = EntryName::new("c d").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(path("/").join(&name).as_str(), "/c d");
        assert_eq!(path("/a/b").join(&name).as_str(), "/a/b/c d");
        let chain: Vec<String> = path("/a/b")
            .ancestors_from_root()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(chain, ["/", "/a", "/a/b"]);
    }

    #[test]
    fn entry_names_cannot_escape() {
        for bad in ["", ".", "..", "a/b", "/", "a\0"] {
            assert!(EntryName::new(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["a", ".hidden", "...", "ä ö", "a\\b"] {
            assert!(EntryName::new(good).is_ok(), "{good:?} must be accepted");
        }
    }
}
