//! What a file is, judged by its kind and name: for the Kind column, the
//! icon in front of it and the color of that icon.

use ferry_core::EntryKind;
use gpui::Hsla;
use gpui_component::Theme;
use gpui_component_assets::IconName;

// The icons used here beyond those gpui-component ships. Only these are
// embedded, not the whole icon catalog.
gpui_component_assets::icon_assets!(
    pub(crate) FileTypeIcons,
    [
        FileArchive,
        FileBraces,
        FileCode,
        FileCog,
        FileImage,
        FileKey,
        FileLock,
        FileMusic,
        FileSpreadsheet,
        FileTerminal,
        FileVideoCamera,
        Disc,
        Package,
        Presentation,
        Link,
        FileQuestionMark,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileType {
    Folder,
    Link,
    Special,
    Pdf,
    Image,
    Video,
    Audio,
    Archive,
    Text,
    Configuration,
    WebPage,
    SourceCode,
    Script,
    Document,
    Spreadsheet,
    Presentation,
    DiskImage,
    Package,
    Key,
    Lock,
    File,
}

/// The palette color an icon is tinted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tint {
    Muted,
    Blue,
    Red,
    Green,
    Yellow,
    Magenta,
    Cyan,
}

impl FileType {
    pub(crate) fn of(name: &str, kind: EntryKind) -> Self {
        match kind {
            EntryKind::Directory => return Self::Folder,
            EntryKind::Symlink => return Self::Link,
            EntryKind::Other => return Self::Special,
            EntryKind::File => {}
        }
        let lower = name.to_ascii_lowercase();
        if [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        {
            return Self::Archive;
        }
        let extension = match lower.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => extension,
            // Dotfiles such as `.bashrc` configure something.
            Some(("", _)) => return Self::Configuration,
            _ => return Self::by_name(&lower),
        };
        match extension {
            "pdf" => Self::Pdf,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "heic"
            | "avif" | "ico" => Self::Image,
            "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" => Self::Video,
            "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" => Self::Audio,
            "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" => Self::Archive,
            "txt" | "md" | "log" | "rst" | "csv" | "tsv" => Self::Text,
            "json" | "yaml" | "yml" | "toml" | "xml" | "ini" | "conf" | "cfg" | "env" => {
                Self::Configuration
            }
            "html" | "htm" | "css" | "scss" => Self::WebPage,
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cpp" | "hpp"
            | "java" | "kt" | "rb" | "php" | "swift" | "lua" | "sql" => Self::SourceCode,
            "sh" | "bash" | "zsh" | "fish" | "ps1" => Self::Script,
            "doc" | "docx" | "odt" | "rtf" | "pages" => Self::Document,
            "xls" | "xlsx" | "ods" | "numbers" => Self::Spreadsheet,
            "ppt" | "pptx" | "odp" | "key" => Self::Presentation,
            "iso" | "img" | "dmg" => Self::DiskImage,
            "deb" | "rpm" | "pkg" | "apk" | "appimage" => Self::Package,
            "pem" | "crt" | "cer" | "pub" | "gpg" | "asc" | "p12" | "pfx" => Self::Key,
            "lock" => Self::Lock,
            _ => Self::File,
        }
    }

    /// Files known by name alone.
    fn by_name(lower: &str) -> Self {
        match lower {
            "makefile" | "dockerfile" | "justfile" | "rakefile" | "gemfile" => Self::SourceCode,
            "readme" | "license" | "copying" | "changelog" => Self::Text,
            name if name.starts_with("id_") => Self::Key,
            _ => Self::File,
        }
    }

    /// For the Kind column.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Folder => "Folder",
            Self::Link => "Link",
            Self::Special => "Special file",
            Self::Pdf => "PDF document",
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archive => "Archive",
            Self::Text => "Text",
            Self::Configuration => "Configuration",
            Self::WebPage => "Web page",
            Self::SourceCode => "Source code",
            Self::Script => "Script",
            Self::Document => "Document",
            Self::Spreadsheet => "Spreadsheet",
            Self::Presentation => "Presentation",
            Self::DiskImage => "Disk image",
            Self::Package => "Package",
            Self::Key => "Key or certificate",
            Self::Lock => "Lock file",
            Self::File => "File",
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::Folder => IconName::Folder,
            Self::Link => IconName::Link,
            Self::Special => IconName::FileQuestionMark,
            Self::Pdf | Self::Text | Self::Document => IconName::FileText,
            Self::Image => IconName::FileImage,
            Self::Video => IconName::FileVideoCamera,
            Self::Audio => IconName::FileMusic,
            Self::Archive => IconName::FileArchive,
            Self::Configuration => IconName::FileCog,
            Self::WebPage | Self::SourceCode => IconName::FileCode,
            Self::Script => IconName::FileTerminal,
            Self::Spreadsheet => IconName::FileSpreadsheet,
            Self::Presentation => IconName::Presentation,
            Self::DiskImage => IconName::Disc,
            Self::Package => IconName::Package,
            Self::Key => IconName::FileKey,
            Self::Lock => IconName::FileLock,
            Self::File => IconName::File,
        }
    }

    fn tint(self) -> Tint {
        match self {
            Self::Folder | Self::Document | Self::Presentation => Tint::Blue,
            Self::Pdf => Tint::Red,
            Self::Image | Self::Video => Tint::Magenta,
            Self::Audio => Tint::Cyan,
            Self::Archive | Self::DiskImage | Self::Package | Self::Key => Tint::Yellow,
            Self::SourceCode | Self::Script | Self::WebPage | Self::Spreadsheet => Tint::Green,
            Self::Link
            | Self::Special
            | Self::Text
            | Self::Configuration
            | Self::Lock
            | Self::File => Tint::Muted,
        }
    }

    /// The icon color in `theme`. Colors are softened so a list of mixed
    /// files reads calm; folders keep their full color.
    pub(crate) fn color(self, theme: &Theme) -> Hsla {
        let color = match self.tint() {
            Tint::Muted => return theme.muted_foreground,
            Tint::Blue => theme.blue,
            Tint::Red => theme.red,
            Tint::Green => theme.green,
            Tint::Yellow => theme.yellow,
            Tint::Magenta => theme.magenta,
            Tint::Cyan => theme.cyan,
        };
        if self == Self::Folder {
            color
        } else {
            color.opacity(0.8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> FileType {
        FileType::of(name, EntryKind::File)
    }

    #[test]
    fn judges_by_extension_and_name() {
        assert_eq!(file("a.PDF"), FileType::Pdf);
        assert_eq!(file("backup.tar.gz"), FileType::Archive);
        assert_eq!(file("main.rs"), FileType::SourceCode);
        assert_eq!(file("deploy.sh"), FileType::Script);
        assert_eq!(file("Cargo.lock"), FileType::Lock);
        assert_eq!(file("cert.pem"), FileType::Key);
        assert_eq!(file("id_ed25519"), FileType::Key);
        assert_eq!(file(".bashrc"), FileType::Configuration);
        assert_eq!(file("Makefile"), FileType::SourceCode);
        assert_eq!(file("notes"), FileType::File);
        assert_eq!(FileType::of("x", EntryKind::Directory), FileType::Folder);
        assert_eq!(FileType::of("x.png", EntryKind::Symlink), FileType::Link);
    }

    #[test]
    fn labels() {
        assert_eq!(file("a.PDF").label(), "PDF document");
        assert_eq!(file("Makefile").label(), "Source code");
        assert_eq!(FileType::of("x", EntryKind::Directory).label(), "Folder");
    }
}
