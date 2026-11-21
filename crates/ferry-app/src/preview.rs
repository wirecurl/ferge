//! Quick look: the selected file, shown over the panes. Space opens and
//! closes it; the keyboard stays in the file list, so the arrow keys move
//! from file to file while it is open.
//!
//! Only the start of a file is read (text up to [`TEXT_LIMIT`], images up to
//! [`IMAGE_LIMIT`]). Reading waits a moment after the selection changed and
//! is abandoned when it changes again, so going through a remote folder
//! quickly does not queue up downloads.

use std::sync::Arc;
use std::time::Duration;

use ferry_core::ops::{Head, read_head};
use ferry_core::{Entry, EntryKind, RemotePath};
use gpui::{
    AnyElement, Context, Entity, Image, ImageFormat, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    div, img, relative, uniform_list,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

use crate::browser::FileBrowser;
use crate::file_type::FileType;
use crate::style::{self, ControlSize, IconButton, TypeScale as _, em};
use crate::{actions, format, runtime};

/// Text read for a preview.
const TEXT_LIMIT: usize = 512 * 1024;
/// Images larger than this are not loaded.
const IMAGE_LIMIT: u64 = 32 * 1024 * 1024;
/// Lines shown at most; longer files end with a note.
const LINE_LIMIT: usize = 10_000;
/// Characters shown of a line at most.
const LINE_WIDTH: usize = 400;
/// How long the selection must rest before the file is read.
const SETTLE: Duration = Duration::from_millis(80);

enum Content {
    /// Nothing selected, or a folder.
    Nothing,
    Loading,
    Text {
        lines: Arc<[SharedString]>,
        /// More of the file exists than is shown.
        cut: bool,
    },
    Image(Arc<Image>),
    /// A file without a preview, with the reason.
    Unavailable(SharedString),
}

pub(crate) struct Preview {
    browser: Option<WeakEntity<FileBrowser>>,
    /// Keeps the subscription to the browser alive.
    observing: Option<Subscription>,
    /// What is shown, as the browser's path and entry.
    shown: Option<(RemotePath, Entry)>,
    content: Content,
    loading: Option<Task<()>>,
    scroll: UniformListScrollHandle,
}

impl Preview {
    pub(crate) fn new() -> Self {
        Self {
            browser: None,
            observing: None,
            shown: None,
            content: Content::Nothing,
            loading: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Follows the selection of `browser`, or shows nothing.
    pub(crate) fn set_browser(
        &mut self,
        browser: Option<&Entity<FileBrowser>>,
        cx: &mut Context<Self>,
    ) {
        self.browser = browser.map(Entity::downgrade);
        self.observing = browser.map(|browser| cx.observe(browser, |this, _, cx| this.sync(cx)));
        self.sync(cx);
    }

    /// Stops following the browser, as when the preview closes.
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_browser(None, cx);
    }

    /// Loads the selected file if it is not the one shown.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .browser
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|browser| {
                let browser = browser.read(cx);
                let entry = browser.selected_entries().into_iter().next()?;
                Some((
                    browser.path().join(&entry.name),
                    entry,
                    Arc::clone(browser.remote()),
                ))
            });
        let Some((path, entry, remote)) = selected else {
            self.shown = None;
            self.content = Content::Nothing;
            self.loading = None;
            cx.notify();
            return;
        };
        if self
            .shown
            .as_ref()
            .is_some_and(|(shown, old)| *shown == path && old.metadata == entry.metadata)
        {
            return;
        }
        self.shown = Some((path.clone(), entry.clone()));
        self.scroll = UniformListScrollHandle::new();
        let file_type = FileType::of(entry.name.as_str(), entry.kind());
        let kind = Kind::of(&entry, file_type);
        let limit = match kind {
            Kind::None(reason) => {
                self.content = reason.map_or(Content::Nothing, |reason| {
                    Content::Unavailable(reason.into())
                });
                self.loading = None;
                cx.notify();
                return;
            }
            Kind::Image(_) => usize::try_from(entry.metadata.size).unwrap_or(usize::MAX),
            Kind::Text => TEXT_LIMIT,
        };
        self.content = Content::Loading;
        cx.notify();
        let io = runtime::handle(cx);
        self.loading = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let reading = io.spawn(async move { read_head(remote.as_ref(), &path, limit).await });
            // Dropping this task (a newer selection) aborts the read.
            let abort = reading.abort_handle();
            let _abort_on_drop = scopeguard(move || abort.abort());
            let content = match reading.await {
                Ok(Ok(head)) => {
                    cx.background_executor()
                        .spawn(async move { decode(kind, &head) })
                        .await
                }
                Ok(Err(error)) => Content::Unavailable(error.to_string().into()),
                Err(_) => return,
            };
            let _ = this.update(cx, |preview, cx| {
                preview.content = content;
                preview.loading = None;
                cx.notify();
            });
        }));
    }
}

/// What to make of a file.
#[derive(Clone, Copy)]
enum Kind {
    Image(ImageFormat),
    Text,
    /// No preview; the reason if there is one worth showing.
    None(Option<&'static str>),
}

impl Kind {
    fn of(entry: &Entry, file_type: FileType) -> Self {
        if entry.kind() != EntryKind::File {
            return Self::None(None);
        }
        if file_type == FileType::Image {
            let lower = entry.name.as_str().to_ascii_lowercase();
            let extension = lower
                .rsplit_once('.')
                .map_or("", |(_, extension)| extension);
            let format = match extension {
                "png" => Some(ImageFormat::Png),
                "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
                "webp" => Some(ImageFormat::Webp),
                "gif" => Some(ImageFormat::Gif),
                "svg" => Some(ImageFormat::Svg),
                "bmp" => Some(ImageFormat::Bmp),
                "tif" | "tiff" => Some(ImageFormat::Tiff),
                "ico" => Some(ImageFormat::Ico),
                _ => None,
            };
            return match format {
                Some(_) if entry.metadata.size > IMAGE_LIMIT => {
                    Self::None(Some("This image is too large to preview."))
                }
                Some(format) => Self::Image(format),
                None => Self::None(Some("This kind of image cannot be previewed.")),
            };
        }
        match file_type {
            FileType::Video
            | FileType::Audio
            | FileType::Archive
            | FileType::DiskImage
            | FileType::Package
            | FileType::Pdf
            | FileType::Document
            | FileType::Spreadsheet
            | FileType::Presentation => Self::None(Some("No preview for this kind of file.")),
            // Anything else may well be text; the content decides.
            _ => Self::Text,
        }
    }
}

/// Turns what was read into something to show.
fn decode(kind: Kind, head: &Head) -> Content {
    match kind {
        Kind::Image(format) => {
            Content::Image(Arc::new(Image::from_bytes(format, head.bytes.to_vec())))
        }
        Kind::Text => text_lines(head).map_or_else(
            || Content::Unavailable("This file is not text.".into()),
            |(lines, cut)| Content::Text { lines, cut },
        ),
        Kind::None(reason) => reason.map_or(Content::Nothing, |reason| {
            Content::Unavailable(reason.into())
        }),
    }
}

/// The lines of `head`, or `None` if it is not UTF-8 text.
fn text_lines(head: &Head) -> Option<(Arc<[SharedString]>, bool)> {
    let bytes = &head.bytes[..];
    if bytes.iter().take(8 * 1024).any(|&byte| byte == 0) {
        return None;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        // A character cut in half at the end of what was read.
        Err(error) if head.truncated && bytes.len() - error.valid_up_to() < 4 => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    let mut cut = head.truncated;
    let mut lines: Vec<SharedString> = Vec::new();
    for line in text.lines() {
        if lines.len() == LINE_LIMIT {
            cut = true;
            break;
        }
        let line = line.replace('\t', "    ");
        let line = match line.char_indices().nth(LINE_WIDTH) {
            Some((end, _)) => format!("{}…", &line[..end]),
            None => line,
        };
        lines.push(line.into());
    }
    Some((lines.into(), cut))
}

/// Runs `action` when dropped.
fn scopeguard(action: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(action) = self.0.take() {
                action();
            }
        }
    }
    Guard(Some(action))
}

impl Render for Preview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, background) = (theme.muted_foreground, theme.border, theme.popover);
        let header = self.shown.as_ref().map(|(_, entry)| {
            let file_type = FileType::of(entry.name.as_str(), entry.kind());
            let mut details = vec![file_type.label().to_owned()];
            if entry.kind() == EntryKind::File {
                details.push(format::size(entry.metadata.size));
            }
            details.push(format::date(entry.metadata.modified));
            h_flex()
                .h(style::HEADER_HEIGHT)
                .px(style::BAR_PADDING)
                .gap_2()
                .flex_shrink_0()
                .border_b_1()
                .border_color(border)
                .child(
                    Icon::new(file_type.icon())
                        .size(em(1.125))
                        .text_color(file_type.color(cx.theme())),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(SharedString::from(entry.name.as_str().to_owned())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_small()
                        .text_color(muted)
                        .child(details.join(" · ")),
                )
                .child(
                    IconButton::new("close-preview", IconName::Close)
                        .size(ControlSize::Compact)
                        .tooltip("Close (Space)")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(actions::Preview), cx);
                        }),
                )
        });
        let body = self.render_body(cx);
        // The backdrop takes clicks beside the card and closes the preview.
        div()
            .id("preview-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .on_click(|_, window, cx| window.dispatch_action(Box::new(actions::Preview), cx))
            .child(
                v_flex()
                    .id("preview")
                    .w(relative(0.8))
                    .h(relative(0.86))
                    .rounded(style::GROUP_RADIUS)
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .shadow_lg()
                    .overflow_hidden()
                    // Clicks on the card stay on it.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .children(header)
                    .child(body),
            )
    }
}

impl Preview {
    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let message = |text: SharedString| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(muted)
                .child(text)
                .into_any_element()
        };
        match &self.content {
            Content::Nothing => message("Select a file to preview it.".into()),
            Content::Loading => message("Loading…".into()),
            Content::Unavailable(reason) => message(reason.clone()),
            Content::Image(image) => div()
                .flex_1()
                .min_h_0()
                .p(style::BAR_PADDING)
                .child(
                    img(Arc::clone(image))
                        .size_full()
                        .object_fit(ObjectFit::ScaleDown)
                        .with_fallback(move || {
                            v_flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .text_color(muted)
                                .child("This image could not be read.")
                                .into_any_element()
                        }),
                )
                .into_any_element(),
            Content::Text { lines, cut } => self.render_text(lines, *cut, cx),
        }
    }

    fn render_text(
        &self,
        lines: &Arc<[SharedString]>,
        cut: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, mono) = (theme.muted_foreground, theme.mono_font_family.clone());
        let shown = Arc::clone(lines);
        let note = cut.then(|| {
            div()
                .px(style::BAR_PADDING)
                .py_1()
                .flex_shrink_0()
                .text_small()
                .text_color(muted)
                .child(format!(
                    "Showing the first {} lines.",
                    format::count(lines.len())
                ))
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list("preview-lines", lines.len(), move |range, _, _| {
                    range
                        .map(|index| {
                            h_flex()
                                .h(em(1.5))
                                .gap_3()
                                .font_family(mono.clone())
                                .text_small()
                                .whitespace_nowrap()
                                .child(
                                    div()
                                        .w(em(3.5))
                                        .flex_shrink_0()
                                        .text_right()
                                        .text_color(muted)
                                        .child((index + 1).to_string()),
                                )
                                .child(shown[index].clone())
                        })
                        .collect()
                })
                .track_scroll(&self.scroll)
                .flex_1()
                .py(em(0.5))
                .px(style::BAR_PADDING),
            )
            .children(note)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &[u8], truncated: bool) -> Head {
        Head {
            bytes: text.to_vec().into(),
            truncated,
        }
    }

    #[test]
    fn splits_text_into_lines() {
        let (lines, cut) = text_lines(&head(b"one\r\ntwo\n\tthree", false)).unwrap();
        assert_eq!(&*lines, ["one", "two", "    three"]);
        assert!(!cut);
    }

    #[test]
    fn tolerates_a_character_cut_by_the_limit() {
        let text = "grüße".as_bytes();
        let (lines, cut) = text_lines(&head(&text[..text.len() - 1], true)).unwrap();
        assert_eq!(&*lines, ["grüß"]);
        assert!(cut);
    }

    #[test]
    fn rejects_binary_data() {
        assert!(text_lines(&head(b"\x7fELF\x00\x01", false)).is_none());
        assert!(text_lines(&head(&[0xff, 0xfe, 0x41], false)).is_none());
    }

    #[test]
    fn shortens_long_lines() {
        let line = "x".repeat(LINE_WIDTH + 10);
        let (lines, _) = text_lines(&head(line.as_bytes(), false)).unwrap();
        assert_eq!(lines[0].chars().count(), LINE_WIDTH + 1);
    }
}
