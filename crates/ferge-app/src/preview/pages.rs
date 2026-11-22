//! PDF documents in the preview: the pages one below the other. A page is
//! rendered when it comes into view; only the pages around it stay in
//! memory.

use std::sync::Arc;

use ferge_core::document::pdf::{Bitmap, PAGE_LIMIT, Pdf};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, ImageSource, IntoElement, ListAlignment, ListState, ObjectFit,
    ParentElement as _, RenderImage, Styled as _, StyledImage as _, Task, WeakEntity, Window, div,
    img, list, px, white,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use super::Preview;
use crate::format;
use crate::style::{self, TypeScale as _, em};

/// How wide pages are shown at most, for comfortable reading.
const PAGE_WIDTH: f32 = 52.0;
/// Rendered pages kept; the ones farthest from the latest are let go.
const KEEP: usize = 12;
/// Pages are rendered at multiples of this width, in pixels, so resizing a
/// window a little does not render them again.
const WIDTH_STEP: f32 = 256.0;

enum Page {
    Waiting,
    Rendering {
        width: u32,
        /// A smaller rendering, shown until this one is ready.
        shown: Option<Arc<RenderImage>>,
        _task: Task<()>,
    },
    Ready {
        image: Arc<RenderImage>,
        width: u32,
    },
    Failed,
}

impl Page {
    /// The image on screen for this page, if any.
    fn image(&self) -> Option<&Arc<RenderImage>> {
        match self {
            Self::Ready { image, .. } => Some(image),
            Self::Rendering { shown, .. } => shown.as_ref(),
            Self::Waiting | Self::Failed => None,
        }
    }

    /// The width rendered or being rendered, 0 for none.
    fn width(&self) -> u32 {
        match self {
            Self::Ready { width, .. } | Self::Rendering { width, .. } => *width,
            Self::Waiting | Self::Failed => 0,
        }
    }
}

pub(super) struct PageView {
    pdf: Arc<Pdf>,
    pages: Vec<Page>,
    list: ListState,
    /// Pages in the order they were rendered, oldest first.
    rendered: Vec<usize>,
}

impl PageView {
    pub(super) fn new(pdf: Pdf) -> Self {
        let count = pdf.pages().len();
        Self {
            pdf: Arc::new(pdf),
            pages: (0..count).map(|_| Page::Waiting).collect(),
            list: ListState::new(count, ListAlignment::Top, px(1200.)),
            rendered: Vec::new(),
        }
    }

    /// The images held, to be released when the view goes.
    pub(super) fn images(&self) -> impl Iterator<Item = Arc<RenderImage>> + '_ {
        self.pages.iter().filter_map(Page::image).cloned()
    }

    /// Starts rendering page `index` at `width` pixels unless it is rendered
    /// or rendering at that size already.
    fn request(&mut self, index: usize, width: u32, cx: &mut Context<Preview>) {
        match self.pages.get(index) {
            None | Some(Page::Failed) => return,
            Some(page) if page.width() >= width => return,
            Some(_) => {}
        }
        let rendering = self.pdf.render(index, width);
        let pdf = Arc::downgrade(&self.pdf);
        let task = cx.spawn(async move |this: WeakEntity<Preview>, cx| {
            let image = match rendering.await {
                Ok(bitmap) => {
                    cx.background_executor()
                        .spawn(async move { to_image(bitmap) })
                        .await
                }
                Err(_) => None,
            };
            let _ = this.update(cx, |preview, cx| {
                let Some(view) = preview.pages_mut() else {
                    return;
                };
                // The document shown may have changed meanwhile.
                if !pdf
                    .upgrade()
                    .is_some_and(|pdf| Arc::ptr_eq(&pdf, &view.pdf))
                {
                    return;
                }
                view.finish(index, width, image, cx);
                cx.notify();
            });
        });
        let shown = match std::mem::replace(&mut self.pages[index], Page::Waiting) {
            Page::Ready { image, .. } => Some(image),
            Page::Rendering { shown, .. } => shown,
            Page::Waiting | Page::Failed => None,
        };
        self.pages[index] = Page::Rendering {
            width,
            shown,
            _task: task,
        };
    }

    fn finish(
        &mut self,
        index: usize,
        width: u32,
        image: Option<Arc<RenderImage>>,
        cx: &mut Context<Preview>,
    ) {
        let new = match image {
            Some(image) => Page::Ready { image, width },
            None => Page::Failed,
        };
        if let Some(old) = std::mem::replace(&mut self.pages[index], new).image() {
            release(Arc::clone(old), cx);
        }
        self.rendered.retain(|&page| page != index);
        self.rendered.push(index);
        // Let go of the rendered pages farthest from this one.
        while self.rendered.len() > KEEP {
            let Some((position, _)) = self
                .rendered
                .iter()
                .enumerate()
                .max_by_key(|(_, page)| page.abs_diff(index))
            else {
                break;
            };
            let page = self.rendered.remove(position);
            if let Some(image) = std::mem::replace(&mut self.pages[page], Page::Waiting).image() {
                release(Arc::clone(image), cx);
            }
        }
    }

    pub(super) fn render(&self, window: &Window, cx: &mut Context<Preview>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let sizes: Arc<[(f32, f32)]> = self
            .pdf
            .pages()
            .iter()
            .map(|page| (page.width, page.height))
            .collect();
        let images: Arc<[Option<Arc<RenderImage>>]> = self
            .pages
            .iter()
            .map(|page| page.image().cloned())
            .collect();
        let widths: Arc<[u32]> = self.pages.iter().map(Page::width).collect();
        let failed: Arc<[bool]> = self
            .pages
            .iter()
            .map(|page| matches!(page, Page::Failed))
            .collect();
        let width = render_width(window);
        let this = cx.entity().downgrade();
        let last = sizes.len().saturating_sub(1);
        let truncated = self.pdf.truncated();
        let pages = list(self.list.clone(), move |index, _, cx: &mut App| {
            if widths[index] < width && !failed[index] {
                let _ = this.update(cx, |preview, cx| {
                    if let Some(view) = preview.pages_mut() {
                        view.request(index, width, cx);
                    }
                });
            }
            let (page_width, page_height) = sizes[index];
            let page = div()
                .w_full()
                .aspect_ratio(page_width.max(1.) / page_height.max(1.))
                .bg(white())
                .border_1()
                .border_color(border)
                .shadow_xs()
                .map(|page| match &images[index] {
                    Some(image) => page.child(
                        img(ImageSource::Render(Arc::clone(image)))
                            .size_full()
                            .object_fit(ObjectFit::Fill),
                    ),
                    None if failed[index] => page.child(
                        v_flex()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .text_color(muted)
                            .child("This page could not be shown."),
                    ),
                    None => page,
                });
            let end = (index == last && truncated).then(|| {
                div()
                    .pt(em(0.75))
                    .text_small()
                    .text_color(muted)
                    .child(format!(
                        "Showing the first {} pages.",
                        format::count(PAGE_LIMIT)
                    ))
            });
            h_flex()
                .w_full()
                .justify_center()
                .px(style::BAR_PADDING)
                .pt(if index == 0 { em(1.) } else { em(0.) })
                .pb(if index == last { em(1.25) } else { em(0.75) })
                .child(
                    v_flex()
                        .w_full()
                        .max_w(em(PAGE_WIDTH))
                        .child(page)
                        .children(end),
                )
                .into_any_element()
        })
        .flex_1()
        .min_h_0();
        pages.into_any_element()
    }
}

/// The width pages are rendered at, in device pixels: as wide as they are
/// shown, in steps.
fn render_width(window: &Window) -> u32 {
    let shown = em(PAGE_WIDTH)
        .to_pixels(window.rem_size())
        .min(window.viewport_size().width * 0.8);
    let pixels = f32::from(shown) * window.scale_factor();
    let stepped = (pixels / WIDTH_STEP).ceil() * WIDTH_STEP;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive width of a few thousand pixels"
    )]
    let width = stepped.clamp(WIDTH_STEP, 4096.0) as u32;
    width
}

/// A rendered page as an image GPUI can draw.
fn to_image(bitmap: Bitmap) -> Option<Arc<RenderImage>> {
    let buffer = super::bgra(bitmap)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// Removes `image` from the GPU once the current frame is done with it.
pub(super) fn release(image: Arc<RenderImage>, cx: &mut App) {
    cx.defer(move |cx| cx.drop_image(image, None));
}
