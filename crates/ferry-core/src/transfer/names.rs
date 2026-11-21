//! New names for "keep both": `report.pdf` → `report (2).pdf`.

use crate::error::{Error, Result};
use crate::path::{EntryName, RemotePath};
use crate::remote::Remote;

/// Compound extensions kept together, so `a.tar.gz` becomes `a (2).tar.gz`.
const COMPOUND_EXTENSIONS: &[&str] = &[".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"];

/// Attempts before giving up on finding a free name.
const MAX_NUMBER: u32 = 10_000;

/// `name` with ` (number)` inserted before its extension.
pub fn numbered_name(name: &str, number: u32) -> String {
    let lower = name.to_ascii_lowercase();
    let split = COMPOUND_EXTENSIONS
        .iter()
        .find(|extension| lower.ends_with(*extension) && lower.len() > extension.len())
        .map(|extension| name.len() - extension.len())
        // A leading dot marks a hidden file, not an extension.
        .or_else(|| name.rfind('.').filter(|&dot| dot > 0))
        .unwrap_or(name.len());
    let (stem, extension) = name.split_at(split);
    format!("{stem} ({number}){extension}")
}

/// The first `name (n).ext` next to `path` that does not exist on `remote`.
pub(super) async fn free_name(remote: &dyn Remote, path: &RemotePath) -> Result<RemotePath> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidPath(path.as_str().into()))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::InvalidPath(path.as_str().into()))?;
    for number in 2..MAX_NUMBER {
        let candidate = parent.join(&EntryName::new(&numbered_name(name, number))?);
        match remote.stat(&candidate).await {
            Err(Error::NotFound { .. }) => return Ok(candidate),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
    }
    Err(Error::AlreadyExists { path: path.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_names_before_the_extension() {
        assert_eq!(numbered_name("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered_name("archive.tar.gz", 3), "archive (3).tar.gz");
        assert_eq!(numbered_name("Makefile", 2), "Makefile (2)");
        assert_eq!(numbered_name(".bashrc", 2), ".bashrc (2)");
        assert_eq!(numbered_name("my.notes.txt", 2), "my.notes (2).txt");
        assert_eq!(numbered_name(".tar.gz", 2), ".tar (2).gz");
    }
}
