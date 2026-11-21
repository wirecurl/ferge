//! Quick look: the selected file, shown over the panes. Space opens and
//! closes it; the keyboard stays in the file list, so the arrow keys move
//! from file to file while it is open.
//!
//! Text, images, spreadsheets ([`sheet`]), office documents as text
//! ([`document`]) and PDF pages ([`pages`]) are shown by Ferge itself; on
//! macOS, Quick Look renders office files as their programs would. Text is
//! read only from the start; documents are read whole, up to a limit.
//! Reading waits a moment after the selection changed and is abandoned when
//! it changes again, so going through a remote folder quickly does not queue
//! up downloads.

mod document;
mod kind;
mod pages;
mod sheet;

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ferge_core::document::pdf::Pdf;
use ferge_core::document::{sheet as sheets, text};
use ferge_core::ops::{Head, read_head};
use ferge_core::{Entry, EntryKind, Remote, RemotePath, quick_look};
use gpui::{
    AnyElement, Context, Entity, Image, ImageFormat, ImageSource, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, Render, RenderImage, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, SvgRenderer,
    Task, UniformListScrollHandle, WeakEntity, Window, div, img, relative, uniform_list,
};
use gpui_base::SelectableText;
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

use self::document::DocumentView;
use self::kind::{DOCUMENT_LIMIT, Kind, Plan};
use self::pages::{PageView, release};
use self::sheet::{Grid, SheetView};
use crate::browser::FileBrowser;
use crate::file_type::FileType;
use crate::style::{self, ControlSize, IconButton, TypeScale as _, em};
use crate::{actions, format, runtime};

/// Lines per block of selectable text. Text is selected and copied within
/// and across blocks; blocks keep the list of lines virtual.
const CHUNK: usize = 64;
/// Lines shown at most; longer files end with a note.
const LINE_LIMIT: usize = 10_000;
/// Characters shown of a line at most.
const LINE_WIDTH: usize = 400;
/// How long the selection must rest before the file is read.
const SETTLE: Duration = Duration::from_millis(80);
/// Pixels on the longer side of a Quick Look picture.
const QUICK_LOOK_SIZE: u32 = 2_400;
/// Images with more pixels than this are not decoded.
const PIXEL_LIMIT: u64 = 120_000_000;

enum Content {
    /// Nothing selected, or a folder.
    Nothing,
    Loading,
    Text {
        /// The lines in blocks of [`CHUNK`], each one selectable text.
        chunks: Arc<[SharedString]>,
        lines: usize,
        /// More of the file exists than is shown.
        cut: bool,
    },
    Image {
        image: Arc<RenderImage>,
        /// Where the picture comes from, if not from the file itself.
        note: Option<&'static str>,
    },
    Sheets(SheetView),
    Document(DocumentView),
    Pages(PageView),
    /// A file without a preview, with the reason.
    Unavailable(SharedString),
}

/// What loading produced, handed from the background to the UI thread.
enum Loaded {
    Text {
        chunks: Arc<[SharedString]>,
        lines: usize,
        cut: bool,
    },
    Image(Arc<RenderImage>, Option<&'static str>),
    Sheets(Vec<Grid>),
    Document(text::Document),
    Pdf(Pdf),
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
            self.loading = None;
            self.set_content(Content::Nothing, cx);
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
        let plan = Plan::of(&entry);
        if let (Kind::None(reason), false) = (plan.kind, plan.quick_look) {
            self.loading = None;
            let content = reason.map_or(Content::Nothing, |reason| {
                Content::Unavailable(reason.into())
            });
            self.set_content(content, cx);
            return;
        }
        self.set_content(Content::Loading, cx);
        let io = runtime::handle(cx);
        let svg = cx.svg_renderer();
        let file = File {
            remote,
            path,
            name: entry.name.as_str().to_owned(),
            size: entry.metadata.size,
        };
        self.loading = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let reading = io.spawn(read(plan, file));
            // Dropping this task (a newer selection) aborts the read.
            let abort = reading.abort_handle();
            let _abort_on_drop = scopeguard(move || abort.abort());
            let Ok(read) = reading.await else {
                return;
            };
            let loaded = cx
                .background_executor()
                .spawn(async move { decode(read, &svg) })
                .await;
            let _ = this.update(cx, |preview, cx| {
                preview.loading = None;
                preview.show(loaded, cx);
            });
        }));
    }

    fn show(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let content = match loaded {
            Loaded::Text { chunks, lines, cut } => Content::Text { chunks, lines, cut },
            Loaded::Image(image, note) => Content::Image { image, note },
            Loaded::Sheets(grids) => Content::Sheets(SheetView::new(grids)),
            Loaded::Document(document) => Content::Document(DocumentView::new(document)),
            Loaded::Pdf(pdf) => Content::Pages(PageView::new(pdf)),
            Loaded::Unavailable(reason) => Content::Unavailable(reason),
        };
        self.set_content(content, cx);
    }

    /// Shows `content`, letting go of the pictures shown before.
    fn set_content(&mut self, content: Content, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.content, content) {
            Content::Image { image, .. } => release(image, cx),
            Content::Pages(view) => {
                for image in view.images().collect::<Vec<_>>() {
                    release(image, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn sheets_mut(&mut self) -> Option<&mut SheetView> {
        match &mut self.content {
            Content::Sheets(view) => Some(view),
            _ => None,
        }
    }

    fn pages_mut(&mut self) -> Option<&mut PageView> {
        match &mut self.content {
            Content::Pages(view) => Some(view),
            _ => None,
        }
    }
}

/// The file to preview.
struct File {
    remote: Arc<dyn Remote>,
    path: RemotePath,
    name: String,
    size: u64,
}

/// What was read, still to be decoded.
enum Read {
    /// A picture of the file from Quick Look.
    QuickLook(Bytes),
    File(Kind, Head),
    Unavailable(SharedString),
}

/// Reads what the preview needs of `file`, asking Quick Look first if the
/// plan says so. Runs on tokio.
async fn read(plan: Plan, file: File) -> Read {
    let mut whole: Option<Head> = None;
    if plan.quick_look {
        let picture = if file.remote.is_local() {
            quick_look::render(Path::new(file.path.as_str()), QUICK_LOOK_SIZE).await
        } else {
            match read_head(file.remote.as_ref(), &file.path, DOCUMENT_LIMIT).await {
                Ok(head) if !head.truncated => {
                    let bytes = head.bytes.clone();
                    whole = Some(head);
                    quick_look::render_bytes(&file.name, bytes, QUICK_LOOK_SIZE).await
                }
                Ok(_) => return Read::Unavailable("This file is too large to preview.".into()),
                Err(error) => return Read::Unavailable(error.to_string().into()),
            }
        };
        match picture {
            Ok(png) => return Read::QuickLook(png),
            Err(error) => tracing::debug!(%error, "no Quick Look picture"),
        }
    }
    if let Kind::None(reason) = plan.kind {
        return Read::Unavailable(reason.unwrap_or("No preview for this kind of file.").into());
    }
    let head = match whole {
        Some(head) => head,
        None => {
            match read_head(
                file.remote.as_ref(),
                &file.path,
                plan.kind.read_limit(file.size),
            )
            .await
            {
                Ok(head) => head,
                Err(error) => return Read::Unavailable(error.to_string().into()),
            }
        }
    };
    Read::File(plan.kind, head)
}

/// Turns what was read into something to show. CPU work, off the UI thread.
fn decode(read: Read, svg: &SvgRenderer) -> Loaded {
    let unavailable = |reason: &str| Loaded::Unavailable(SharedString::from(reason.to_owned()));
    let (kind, head) = match read {
        Read::QuickLook(png) => {
            return match image(ImageFormat::Png, &png, svg) {
                Some(image) => Loaded::Image(image, Some("First page, as Quick Look shows it.")),
                None => unavailable("Quick Look gave no picture of this file."),
            };
        }
        Read::Unavailable(reason) => return Loaded::Unavailable(reason),
        Read::File(kind, head) => (kind, head),
    };
    let whole = matches!(kind, Kind::Workbook(_) | Kind::Document(_) | Kind::Pdf);
    if whole && head.truncated {
        return unavailable("This file is too large to preview.");
    }
    match kind {
        Kind::Image(format) => image(format, &head.bytes, svg).map_or_else(
            || unavailable("This image could not be read."),
            |image| Loaded::Image(image, None),
        ),
        Kind::Text => text_lines(&head).map_or_else(
            || unavailable("This file is not text."),
            |(lines, cut)| Loaded::Text {
                chunks: chunks(&lines),
                lines: lines.len(),
                cut,
            },
        ),
        Kind::Delimited(delimiter) => {
            let text = String::from_utf8_lossy(&head.bytes);
            let sheet = sheets::delimited(&text, delimiter, head.truncated);
            Loaded::Sheets(vec![Grid::new(sheet)])
        }
        Kind::Workbook(format) => match sheets::workbook(&head.bytes, format) {
            Ok(found) => Loaded::Sheets(found.into_iter().map(Grid::new).collect()),
            Err(error) => unavailable(&error.to_string()),
        },
        Kind::Document(format) => match text::document(&head.bytes, format) {
            Ok(document) => Loaded::Document(document),
            Err(error) => unavailable(&error.to_string()),
        },
        Kind::Pdf => match Pdf::open(head.bytes) {
            Ok(pdf) => Loaded::Pdf(pdf),
            Err(error) => unavailable(&error.to_string()),
        },
        Kind::None(reason) => unavailable(reason.unwrap_or("No preview for this kind of file.")),
    }
}

/// Decodes an image, unless it is damaged or has more pixels than is sane
/// to hold.
fn image(format: ImageFormat, bytes: &[u8], svg: &SvgRenderer) -> Option<Arc<RenderImage>> {
    if format != ImageFormat::Svg {
        let (width, height) = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()?;
        if u64::from(width) * u64::from(height) > PIXEL_LIMIT {
            return None;
        }
    }
    Image::from_bytes(format, bytes.to_vec())
        .to_image_data(svg.clone())
        .ok()
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

/// The lines joined into blocks of [`CHUNK`] lines.
fn chunks(lines: &[SharedString]) -> Arc<[SharedString]> {
    lines
        .chunks(CHUNK)
        .map(|chunk| SharedString::from(chunk.join("\n")))
        .collect()
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let body = self.render_body(window, cx);
        // The backdrop takes clicks beside the card and closes the preview.
        // It occludes the file list, so no click, drag or scroll reaches it.
        div()
            .id("preview-backdrop")
            .absolute()
            .inset_0()
            .occlude()
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
    fn render_body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
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
            Content::Image { image, note } => v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    div().flex_1().min_h_0().p(style::BAR_PADDING).child(
                        img(ImageSource::Render(Arc::clone(image)))
                            .size_full()
                            .object_fit(ObjectFit::ScaleDown),
                    ),
                )
                .children(note.map(|note| {
                    div()
                        .px(style::BAR_PADDING)
                        .py_1()
                        .text_small()
                        .text_color(muted)
                        .child(note)
                }))
                .into_any_element(),
            Content::Sheets(view) => view.render(cx),
            Content::Document(view) => view.render(cx),
            Content::Pages(view) => view.render(window, cx),
            Content::Text { chunks, lines, cut } => self.render_text(chunks, *lines, *cut, cx),
        }
    }

    fn render_text(
        &self,
        chunks: &Arc<[SharedString]>,
        lines: usize,
        cut: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, mono) = (theme.muted_foreground, theme.mono_font_family.clone());
        let shown = Arc::clone(chunks);
        let note = cut.then(|| {
            div()
                .px(style::BAR_PADDING)
                .py_1()
                .flex_shrink_0()
                .text_small()
                .text_color(muted)
                .child(format!("Showing the first {} lines.", format::count(lines)))
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list("preview-lines", chunks.len(), move |range, _, _| {
                    range
                        .map(|index| {
                            let first = index * CHUNK + 1;
                            let last = (first + CHUNK - 1).min(lines);
                            let numbers = (first..=last)
                                .map(|number| number.to_string())
                                .collect::<Vec<_>>()
                                .join("\n");
                            h_flex()
                                .items_start()
                                .gap_3()
                                .font_family(mono.clone())
                                .text_small()
                                .line_height(em(1.5))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .w(em(3.5))
                                        .flex_shrink_0()
                                        .text_right()
                                        .text_color(muted)
                                        .child(numbers),
                                )
                                .child(
                                    SelectableText::new(
                                        ("preview-text", index),
                                        shown[index].clone(),
                                    )
                                    .document_order(index as u64),
                                )
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
    use gpui::{
        AppContext as _, Modifiers, MouseButton, Pixels, TestAppContext, VisualTestContext, point,
        px,
    };
    use gpui_base::TextSelectionLayer;

    use super::*;

    /// The preview over a window, with the selection layer Root provides.
    struct Host(Entity<Preview>);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .relative()
                .child(TextSelectionLayer)
                .child(self.0.clone())
        }
    }

    fn drag(cx: &mut VisualTestContext, from: gpui::Point<Pixels>, to: gpui::Point<Pixels>) {
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    }

    #[gpui::test]
    fn previewed_text_can_be_selected(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let lines: Vec<SharedString> = ["alpha beta", "", "gamma"]
            .into_iter()
            .map(SharedString::from)
            .collect();
        let preview = cx.new(|_| {
            let mut preview = Preview::new();
            preview.content = Content::Text {
                chunks: chunks(&lines),
                lines: lines.len(),
                cut: false,
            };
            preview
        });
        let (_, cx) = cx.add_window_view(|_, _| Host(preview.clone()));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        // Find the first line: the card is centered, the text is in it,
        // right of the line numbers.
        let (viewport, rem) = cx.update(|window, _| (window.viewport_size(), window.rem_size()));
        let x = viewport.width * 0.1 + style::BAR_PADDING.to_pixels(rem) + em(4.5).to_pixels(rem);
        let top = (0_u16..400)
            .map(|step| viewport.height * 0.07 + px(f32::from(step)))
            .find(|&y| {
                // Hit testing follows the pointer.
                cx.simulate_mouse_move(point(x, y), None, Modifiers::default());
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                    gpui_base::TextSelection::is_selectable_at(point(x, y), window, cx)
                })
            })
            .expect("the text is shown");
        let line = em(1.5).to_pixels(rem);
        drag(
            cx,
            point(x, top + line * 0.5),
            point(viewport.width * 0.85, top + line * 2.5),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            assert_eq!(
                gpui_base::TextSelection::selected_text(window, cx),
                "alpha beta\n\ngamma"
            );
        });
    }

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
