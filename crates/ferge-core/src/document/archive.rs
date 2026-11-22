//! The ZIP container of Office Open XML and OpenDocument files, opened with
//! care: an archive is a few bytes on disk that may unpack to gigabytes.

use std::io::{Cursor, Read as _};

use zip::ZipArchive;

use super::{DocumentError, is_compound_file};

/// Entries an archive may have at most.
const ENTRY_LIMIT: usize = 20_000;
/// What all entries together may unpack to, as their headers state.
const UNPACKED_LIMIT: u64 = 512 * 1024 * 1024;
/// How much larger than its packed size a large entry may unpack to. Office
/// XML packs to about a tenth; archives built to explode pack far tighter.
const RATIO_LIMIT: u64 = 200;
/// Entries above this size are held to [`RATIO_LIMIT`].
const RATIO_FROM: u64 = 1024 * 1024;
/// Bytes read of a single entry at most, whatever its header says.
const ENTRY_READ_LIMIT: u64 = 128 * 1024 * 1024;

pub(super) struct Archive<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
    /// What may still be unpacked, over all reads: a document that names
    /// the same large entry again and again cannot unpack it again and
    /// again.
    budget: u64,
}

impl<'a> Archive<'a> {
    /// Opens `bytes` as a ZIP archive whose entries are within the limits.
    pub(super) fn open(bytes: &'a [u8]) -> Result<Self, DocumentError> {
        if is_compound_file(bytes) {
            // Office keeps password-protected documents in a compound file.
            return Err(DocumentError::Encrypted);
        }
        let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
            tracing::debug!(%error, "not a ZIP archive");
            DocumentError::Damaged
        })?;
        check(&mut zip)?;
        Ok(Self {
            zip,
            budget: UNPACKED_LIMIT,
        })
    }

    /// The entry `name` as text, or `None` if there is no such entry.
    pub(super) fn text(&mut self, name: &str) -> Result<Option<String>, DocumentError> {
        let Some(bytes) = self.read(name, ENTRY_READ_LIMIT, true)? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes).map_err(|_| DocumentError::Damaged)?;
        Ok(Some(match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_owned(),
            None => text,
        }))
    }

    /// Up to `limit` bytes from the start of entry `name`, or `None` if
    /// there is no such entry.
    pub(super) fn head(
        &mut self,
        name: &str,
        limit: u64,
    ) -> Result<Option<Vec<u8>>, DocumentError> {
        self.read(name, limit, false)
    }

    /// Reads entry `name`, at most `limit` bytes. With `whole`, an entry
    /// longer than that is an error; otherwise it is cut.
    fn read(
        &mut self,
        name: &str,
        limit: u64,
        whole: bool,
    ) -> Result<Option<Vec<u8>>, DocumentError> {
        let mut entry = match self.zip.by_name(name) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(error) => {
                tracing::debug!(%error, name, "unreadable archive entry");
                return Err(DocumentError::Damaged);
            }
        };
        let allowed = limit.min(self.budget);
        let mut bytes = Vec::new();
        (&mut entry)
            .take(allowed.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| {
                tracing::debug!(%error, name, "unreadable archive entry");
                DocumentError::Damaged
            })?;
        let read = bytes.len() as u64;
        if read > allowed {
            if whole || allowed < limit {
                return Err(DocumentError::TooLarge);
            }
            bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        self.budget = self.budget.saturating_sub(read.min(allowed));
        Ok(Some(bytes))
    }
}

/// Rejects archives with too many entries, too much content or entries that
/// unpack far beyond what documents do, before anything is unpacked.
fn check(zip: &mut ZipArchive<Cursor<&[u8]>>) -> Result<(), DocumentError> {
    if zip.len() > ENTRY_LIMIT {
        return Err(DocumentError::TooLarge);
    }
    let mut total: u64 = 0;
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).map_err(|error| {
            tracing::debug!(%error, index, "unreadable archive entry");
            DocumentError::Damaged
        })?;
        if entry.encrypted() {
            return Err(DocumentError::Encrypted);
        }
        let (size, packed) = (entry.size(), entry.compressed_size());
        if size > RATIO_FROM && size / packed.max(1) > RATIO_LIMIT {
            return Err(DocumentError::TooLarge);
        }
        total = total.saturating_add(size);
        if total > UNPACKED_LIMIT {
            return Err(DocumentError::TooLarge);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::Write as _;

    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;

    /// A ZIP archive holding `entries`, deflated.
    pub(in crate::document) fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_entries_as_text() {
        let bytes = zip(&[("a.xml", "\u{feff}<a>ä</a>".as_bytes())]);
        let mut archive = Archive::open(&bytes).unwrap();
        assert_eq!(archive.text("a.xml").unwrap().unwrap(), "<a>ä</a>");
        assert_eq!(archive.text("missing.xml").unwrap(), None);
    }

    #[test]
    fn rejects_archives_that_unpack_far_beyond_their_size() {
        let bomb = vec![0_u8; 8 * 1024 * 1024];
        let bytes = zip(&[("content.xml", &bomb)]);
        assert!(bytes.len() < 64 * 1024);
        assert_eq!(Archive::open(&bytes).err(), Some(DocumentError::TooLarge));
    }

    #[test]
    fn reading_the_same_entry_again_uses_up_the_budget() {
        let content = vec![b'a'; 1024 * 1024];
        let bytes = zip(&[("a.xml", &content)]);
        let mut archive = Archive::open(&bytes).unwrap();
        let mut reads = 0;
        while archive.text("a.xml").is_ok() {
            reads += 1;
            assert!(reads <= 512, "the budget must run out");
        }
        assert_eq!(archive.text("a.xml").err(), Some(DocumentError::TooLarge));
        assert!(reads >= 511);
    }

    #[test]
    fn reads_the_head_of_an_entry() {
        let bytes = zip(&[("a.xml", b"0123456789")]);
        let mut archive = Archive::open(&bytes).unwrap();
        assert_eq!(archive.head("a.xml", 4).unwrap().unwrap(), b"0123");
        assert_eq!(archive.head("a.xml", 40).unwrap().unwrap(), b"0123456789");
        assert_eq!(archive.head("missing", 4).unwrap(), None);
    }

    #[test]
    fn tells_protected_and_damaged_files_apart() {
        let compound = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 0, 0];
        assert_eq!(
            Archive::open(&compound).err(),
            Some(DocumentError::Encrypted)
        );
        assert_eq!(
            Archive::open(b"PK not really").err(),
            Some(DocumentError::Damaged)
        );
    }
}
