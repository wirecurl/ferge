//! The application's assets: gpui-component's icons and the icons Ferry uses
//! beyond them. Only these are embedded, not the whole icon catalog; the
//! tests check that every icon in use is among them.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

gpui_component_assets::icon_assets!(
    pub(crate) AppIcons,
    [
        // File types.
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
        // Commands.
        X,
        Unplug,
        FolderPlus,
        FilePlus,
        Pencil,
        Trash,
        ClipboardPaste,
        ListChecks,
        ArrowUpDown,
        PanelsTopLeft,
        Clock,
        ShieldUser,
    ]
);

pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = AppIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut paths = gpui_component_assets::Assets.list(path)?;
        paths.extend(AppIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails for an icon that would draw as nothing.
    fn assert_embedded(icon: gpui_component_assets::IconName) {
        let path = icon.path();
        assert!(
            Assets.load(&path).unwrap().is_some(),
            "{path} is not embedded; add it to AppIcons"
        );
    }

    #[test]
    fn file_type_icons_are_embedded() {
        for file_type in crate::file_type::FileType::ALL {
            assert_embedded(file_type.icon());
        }
    }

    #[test]
    fn command_icons_are_embedded() {
        for (_, _, _, icon) in crate::workspace::palette_commands() {
            assert_embedded(icon);
        }
    }
}
