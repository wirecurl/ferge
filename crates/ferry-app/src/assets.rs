//! The application's assets: gpui-component's icons and the file type icons.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

use crate::file_type::FileTypeIcons;

pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = FileTypeIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut paths = gpui_component_assets::Assets.list(path)?;
        paths.extend(FileTypeIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
