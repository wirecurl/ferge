//! What a preview makes of a file, judged by its name and size.

use ferge_core::document::sheet::WorkbookFormat;
use ferge_core::document::text::TextFormat;
use ferge_core::{Entry, EntryKind};
use gpui::ImageFormat;

use crate::file_type::FileType;

/// Text read for a preview.
pub(super) const TEXT_LIMIT: usize = 512 * 1024;
/// Delimited text read for a table.
pub(super) const TABLE_TEXT_LIMIT: usize = 8 * 1024 * 1024;
/// Images larger than this are not loaded.
const IMAGE_LIMIT: u64 = 32 * 1024 * 1024;
/// Documents are read whole, up to this size.
pub(super) const DOCUMENT_LIMIT: usize = 64 * 1024 * 1024;

/// How a file is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Image(ImageFormat),
    Text,
    /// Comma, semicolon or tab separated values; `None` finds out which.
    Delimited(Option<char>),
    Workbook(WorkbookFormat),
    Document(TextFormat),
    Pdf,
    /// No preview; the reason if there is one worth showing.
    None(Option<&'static str>),
}

/// How a file is previewed: Quick Look first where the system has it, then
/// Ferge's own reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Plan {
    pub kind: Kind,
    pub quick_look: bool,
}

impl Kind {
    /// The bytes read from the start of the file.
    pub(super) fn read_limit(self, size: u64) -> usize {
        match self {
            Self::Image(_) => usize::try_from(size).unwrap_or(usize::MAX),
            Self::Text | Self::None(_) => TEXT_LIMIT,
            Self::Delimited(_) => TABLE_TEXT_LIMIT,
            Self::Workbook(_) | Self::Document(_) | Self::Pdf => DOCUMENT_LIMIT,
        }
    }
}

impl Plan {
    pub(super) fn of(entry: &Entry) -> Self {
        let none = |reason| Self {
            kind: Kind::None(reason),
            quick_look: false,
        };
        if entry.kind() != EntryKind::File {
            return none(None);
        }
        let file_type = FileType::of(entry.name.as_str(), entry.kind());
        let lower = entry.name.as_str().to_ascii_lowercase();
        let extension = lower
            .rsplit_once('.')
            .map_or("", |(_, extension)| extension);
        let office = matches!(
            file_type,
            FileType::Document | FileType::Spreadsheet | FileType::Presentation
        ) || matches!(extension, "xlsb" | "xlsm" | "docm" | "pptm");
        // The system renders office files as their programs would.
        let quick_look = cfg!(target_os = "macos") && office;
        let kind = match extension {
            "csv" => Kind::Delimited(None),
            "tsv" | "tab" => Kind::Delimited(Some('\t')),
            "xlsx" | "xlsm" => Kind::Workbook(WorkbookFormat::Xlsx),
            "xlsb" => Kind::Workbook(WorkbookFormat::Xlsb),
            "ods" => Kind::Workbook(WorkbookFormat::Ods),
            "docx" | "docm" => Kind::Document(TextFormat::Docx),
            "pptx" | "pptm" => Kind::Document(TextFormat::Pptx),
            "odt" => Kind::Document(TextFormat::Odt),
            "odp" => Kind::Document(TextFormat::Odp),
            "pdf" => Kind::Pdf,
            "doc" | "xls" | "ppt" => Kind::None(Some(
                "Older Office files (.doc, .xls, .ppt) cannot be previewed.",
            )),
            "pages" | "numbers" | "key" => Kind::None(Some(
                "Pages, Numbers and Keynote files can be previewed on a Mac.",
            )),
            "rtf" => Kind::None(Some("Rich text files cannot be previewed yet.")),
            _ if file_type == FileType::Image => image_kind(extension, entry.metadata.size),
            _ => match file_type {
                FileType::Video
                | FileType::Audio
                | FileType::Archive
                | FileType::DiskImage
                | FileType::Package => Kind::None(Some("No preview for this kind of file.")),
                // Anything else may well be text; the content decides.
                _ => Kind::Text,
            },
        };
        let kind = match kind {
            Kind::Workbook(_) | Kind::Document(_) | Kind::Pdf
                if entry.metadata.size > DOCUMENT_LIMIT as u64 =>
            {
                Kind::None(Some("This file is too large to preview."))
            }
            kind => kind,
        };
        Self { kind, quick_look }
    }
}

fn image_kind(extension: &str, size: u64) -> Kind {
    let format = match extension {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        _ => return Kind::None(Some("This kind of image cannot be previewed.")),
    };
    if size > IMAGE_LIMIT {
        Kind::None(Some("This image is too large to preview."))
    } else {
        Kind::Image(format)
    }
}

#[cfg(test)]
mod tests {
    use ferge_core::{EntryName, Metadata};

    use super::*;

    fn plan(name: &str, size: u64) -> Plan {
        Plan::of(&Entry {
            name: EntryName::new(name).unwrap(),
            metadata: Metadata {
                kind: EntryKind::File,
                size,
                modified: None,
                permissions: None,
            },
        })
    }

    #[test]
    fn picks_a_reader_by_name() {
        assert_eq!(plan("Kosten.CSV", 10).kind, Kind::Delimited(None));
        assert_eq!(plan("a.tsv", 10).kind, Kind::Delimited(Some('\t')));
        assert_eq!(
            plan("Plan.xlsx", 10).kind,
            Kind::Workbook(WorkbookFormat::Xlsx)
        );
        assert_eq!(plan("Brief.odt", 10).kind, Kind::Document(TextFormat::Odt));
        assert_eq!(
            plan("Folien.pptx", 10).kind,
            Kind::Document(TextFormat::Pptx)
        );
        assert_eq!(plan("scan.pdf", 10).kind, Kind::Pdf);
        assert_eq!(plan("main.rs", 10).kind, Kind::Text);
        assert_eq!(plan("photo.jpg", 10).kind, Kind::Image(ImageFormat::Jpeg));
        assert!(matches!(plan("old.doc", 10).kind, Kind::None(Some(_))));
        assert!(matches!(plan("movie.mkv", 10).kind, Kind::None(Some(_))));
    }

    #[test]
    fn leaves_out_files_too_large_to_read() {
        let huge = DOCUMENT_LIMIT as u64 + 1;
        assert!(matches!(plan("big.pdf", huge).kind, Kind::None(Some(_))));
        assert!(matches!(
            plan("big.png", 33 << 20).kind,
            Kind::None(Some(_))
        ));
        // Text is only read from the start, whatever its size.
        assert_eq!(plan("big.log", huge).kind, Kind::Text);
    }

    #[test]
    fn asks_quick_look_for_office_files_on_a_mac() {
        let mac = cfg!(target_os = "macos");
        assert_eq!(plan("Plan.xlsx", 10).quick_look, mac);
        assert_eq!(plan("old.doc", 10).quick_look, mac);
        assert!(!plan("scan.pdf", 10).quick_look);
        assert!(!plan("main.rs", 10).quick_look);
    }
}
