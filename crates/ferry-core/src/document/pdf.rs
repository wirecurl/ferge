//! PDF pages as bitmaps, rendered with hayro (pure Rust, on the CPU).
//!
//! A [`Pdf`] owns a thread that holds the parsed document and renders pages
//! on request, one at a time. Requests whose answer is no longer awaited are
//! skipped, so scrolling past pages does not queue up work. The thread ends
//! when the [`Pdf`] is dropped.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, mpsc};

use bytes::Bytes;
use futures::channel::oneshot;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::{LoadPdfError, Pdf as Document};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::peniko::ImageAlphaType;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

use super::DocumentError;

/// Pages listed at most.
pub const PAGE_LIMIT: usize = 1_000;
/// The tallest bitmap rendered, in pixels; taller pages render smaller.
const MAX_HEIGHT: f32 = 16_384.0;
/// Widths a page may be rendered at, in pixels.
const WIDTHS: std::ops::RangeInclusive<u32> = 16..=4_096;

/// The size of a page in points (1/72 inch), as shown (rotation applied).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// A rendered page: opaque RGBA, 8 bits per channel, row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct Request {
    page: usize,
    width: u32,
    reply: oneshot::Sender<Result<Bitmap, DocumentError>>,
}

/// An open PDF document.
#[derive(Debug)]
pub struct Pdf {
    pages: Arc<[PageSize]>,
    /// More pages exist than [`PAGE_LIMIT`].
    truncated: bool,
    requests: mpsc::Sender<Request>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("page", &self.page)
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

impl Pdf {
    /// Parses `bytes` on a new rendering thread. Blocks until the document is
    /// read; call it off the UI thread.
    pub fn open(bytes: Bytes) -> Result<Self, DocumentError> {
        let (requests, incoming) = mpsc::channel::<Request>();
        let (opened, opening) = mpsc::channel();
        std::thread::Builder::new()
            .name("ferry-pdf".to_owned())
            .spawn(move || {
                let document = match catch_unwind(|| Document::new(Arc::new(bytes))) {
                    Ok(Ok(document)) => document,
                    Ok(Err(LoadPdfError::Decryption(_))) => {
                        let _ = opened.send(Err(DocumentError::Encrypted));
                        return;
                    }
                    Ok(Err(LoadPdfError::Invalid)) | Err(_) => {
                        let _ = opened.send(Err(DocumentError::Damaged));
                        return;
                    }
                };
                let pages = document.pages();
                let sizes: Vec<PageSize> = pages
                    .iter()
                    .take(PAGE_LIMIT)
                    .map(|page| {
                        let (width, height) = page.render_dimensions();
                        PageSize { width, height }
                    })
                    .collect();
                if sizes.is_empty() {
                    let _ = opened.send(Err(DocumentError::Damaged));
                    return;
                }
                if opened.send(Ok((sizes, pages.len() > PAGE_LIMIT))).is_err() {
                    return;
                }
                serve(&document, &incoming);
            })
            .map_err(|error| {
                tracing::warn!(%error, "cannot start the PDF renderer");
                DocumentError::Damaged
            })?;
        let (pages, truncated) = opening.recv().map_err(|_| DocumentError::Damaged)??;
        Ok(Self {
            pages: pages.into(),
            truncated,
            requests,
        })
    }

    /// The pages, up to [`PAGE_LIMIT`].
    pub fn pages(&self) -> &[PageSize] {
        &self.pages
    }

    /// Whether the document has more pages than [`Self::pages`].
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Renders page `page` (from 0) `width` pixels wide. Dropping the future
    /// before it finishes skips the work if it has not started.
    pub fn render(
        &self,
        page: usize,
        width: u32,
    ) -> impl Future<Output = Result<Bitmap, DocumentError>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        let sent = self.requests.send(Request { page, width, reply }).is_ok();
        async move {
            if !sent {
                return Err(DocumentError::Damaged);
            }
            answer.await.unwrap_or(Err(DocumentError::Damaged))
        }
    }
}

/// Renders requested pages until the [`Pdf`] is dropped.
fn serve(document: &Document, requests: &mpsc::Receiver<Request>) {
    let cache = RenderCache::new();
    let settings = InterpreterSettings::default();
    let pages = document.pages();
    while let Ok(request) = requests.recv() {
        if request.reply.is_canceled() {
            continue;
        }
        let result = pages
            .get(request.page)
            .ok_or(DocumentError::Damaged)
            .and_then(|page| {
                // hayro is young; a page it trips over must not take the
                // renderer down with it.
                catch_unwind(AssertUnwindSafe(|| {
                    render_page(page, &cache, &settings, request.width)
                }))
                .map_err(|_| DocumentError::Damaged)
            });
        let _ = request.reply.send(result);
    }
}

fn render_page<'a>(
    page: &'a hayro::hayro_syntax::page::Page<'a>,
    cache: &RenderCache<'a>,
    settings: &InterpreterSettings,
    width: u32,
) -> Bitmap {
    let (page_width, page_height) = page.render_dimensions();
    #[expect(
        clippy::cast_precision_loss,
        reason = "widths are clamped to a few thousand pixels"
    )]
    let wanted = width.clamp(*WIDTHS.start(), *WIDTHS.end()) as f32;
    let scale = (wanted / page_width.max(1.0)).min(MAX_HEIGHT / page_height.max(1.0));
    let pixmap = hayro::render(
        page,
        cache,
        settings,
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    Bitmap {
        width: u32::from(pixmap.width()),
        height: u32::from(pixmap.height()),
        rgba: pixmap.take_rgba8(ImageAlphaType::Alpha),
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    /// A one-page PDF, 200 × 100 points, with a black square in the corner.
    fn square() -> Bytes {
        let content = "0 0 0 rg 0 0 50 50 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            write!(pdf, "{} 0 obj\n{object}\nendobj\n", index + 1).unwrap();
        }
        let xref = pdf.len();
        write!(pdf, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        write!(
            pdf,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .unwrap();
        Bytes::from(pdf)
    }

    #[test]
    fn renders_pages() {
        let pdf = Pdf::open(square()).unwrap();
        assert_eq!(
            pdf.pages(),
            [PageSize {
                width: 200.0,
                height: 100.0
            }]
        );
        assert!(!pdf.truncated());
        let bitmap = futures::executor::block_on(pdf.render(0, 400)).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (400, 200));
        assert_eq!(bitmap.rgba.len(), 400 * 200 * 4);
        let pixel = |x: usize, y: usize| &bitmap.rgba[(y * 400 + x) * 4..][..4];
        // The square sits at the bottom left; the rest is white paper.
        assert_eq!(pixel(10, 190), [0, 0, 0, 255]);
        assert_eq!(pixel(390, 10), [255, 255, 255, 255]);
    }

    #[test]
    fn reports_missing_pages_and_damaged_files() {
        let pdf = Pdf::open(square()).unwrap();
        assert_eq!(
            futures::executor::block_on(pdf.render(5, 100)),
            Err(DocumentError::Damaged)
        );
        assert_eq!(
            Pdf::open(Bytes::from_static(b"%PDF-1.4 nothing")).err(),
            Some(DocumentError::Damaged)
        );
    }
}
