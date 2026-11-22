//! Pictures for previews: raster images (PNG, JPEG, GIF, WebP, BMP, TIFF,
//! ICO and the like) and SVG drawings, decoded to RGBA bitmaps.
//!
//! The files may come from anywhere, so decoding is bounded: images with
//! more pixels than [`PIXEL_LIMIT`] are refused before they are decoded,
//! large ones are scaled down to [`MAX_SIDE`], animations keep only as many
//! frames as fit [`FRAME_BUDGET`], and an SVG never reads anything but
//! itself (no files, no fonts named by the drawing). The preview runs this
//! in the worker process (see [`super::worker`]), which also limits memory
//! and time.

use std::io::Cursor;

use image::{AnimationDecoder as _, DynamicImage, ImageFormat, ImageReader, Limits};

use super::{Bitmap, DocumentError};

/// Images with more pixels than this are not decoded.
pub const PIXEL_LIMIT: u64 = 120_000_000;
/// The longer side of a decoded picture at most, in pixels; larger
/// pictures are scaled down, which the preview does on screen anyway.
pub const MAX_SIDE: u32 = 8_192;
/// The longer side SVG drawings are rendered at.
const SVG_SIDE: f32 = 2_048.0;
/// Bytes of all frames of an animation together at most. Frames past it
/// are left out; the animation then ends early.
const FRAME_BUDGET: usize = 256 * 1024 * 1024;
/// Frames of an animation at most.
const FRAME_LIMIT: usize = 1_000;
/// What the image decoders may allocate for one image.
const DECODER_ALLOCATION: u64 = 1024 * 1024 * 1024;

/// One frame of a picture and how long it is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub bitmap: Bitmap,
    /// How long to show this frame of an animation, in milliseconds.
    pub delay_ms: u32,
}

/// A decoded picture: one frame, or the frames of an animation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub frames: Vec<Frame>,
}

/// Decodes `bytes`: an SVG drawing if `svg` is set, otherwise a raster
/// image of whatever format its content shows.
pub fn decode(bytes: &[u8], svg: bool) -> Result<Picture, DocumentError> {
    if svg {
        return drawing(bytes).map(|bitmap| Picture {
            frames: vec![Frame {
                bitmap,
                delay_ms: 0,
            }],
        });
    }
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| DocumentError::Damaged)?;
    let format = reader.format().ok_or(DocumentError::Damaged)?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| DocumentError::Damaged)?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels > PIXEL_LIMIT {
        return Err(DocumentError::TooLarge);
    }
    // An animation keeps its frames if one frame at full size fits the
    // budget several times over; a huge canvas is shown still.
    let frame_bytes = usize::try_from(pixels.saturating_mul(4)).unwrap_or(usize::MAX);
    if frame_bytes.saturating_mul(4) <= FRAME_BUDGET
        && let Some(frames) = animation(bytes, format, frame_bytes)?
    {
        return Ok(Picture { frames });
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits());
    let image = reader.decode().map_err(|_| DocumentError::Damaged)?;
    Ok(Picture {
        frames: vec![Frame {
            bitmap: bitmap(scaled(image)),
            delay_ms: 0,
        }],
    })
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(DECODER_ALLOCATION);
    limits
}

/// Scales `image` down so that neither side exceeds [`MAX_SIDE`].
fn scaled(image: DynamicImage) -> DynamicImage {
    if image.width().max(image.height()) <= MAX_SIDE {
        image
    } else {
        image.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle)
    }
}

fn bitmap(image: DynamicImage) -> Bitmap {
    let rgba = image.into_rgba8();
    Bitmap {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

/// The frames of an animated GIF or WebP, or `None` if `bytes` hold a still
/// picture (or a format without animations here).
fn animation(
    bytes: &[u8],
    format: ImageFormat,
    frame_bytes: usize,
) -> Result<Option<Vec<Frame>>, DocumentError> {
    let frames = match format {
        ImageFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                .map_err(|_| DocumentError::Damaged)?;
            image::ImageDecoder::set_limits(&mut decoder, limits())
                .map_err(|_| DocumentError::TooLarge)?;
            decoder.into_frames()
        }
        ImageFormat::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|_| DocumentError::Damaged)?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            image::ImageDecoder::set_limits(&mut decoder, limits())
                .map_err(|_| DocumentError::TooLarge)?;
            decoder.into_frames()
        }
        _ => return Ok(None),
    };
    let mut out = Vec::new();
    let mut used = 0_usize;
    for frame in frames {
        if out.len() == FRAME_LIMIT || used.saturating_add(frame_bytes) > FRAME_BUDGET {
            break;
        }
        let frame = frame.map_err(|_| DocumentError::Damaged)?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay_ms = numerator.checked_div(denominator).unwrap_or(0);
        let buffer = frame.into_buffer();
        used = used.saturating_add(buffer.as_raw().len());
        out.push(Frame {
            bitmap: Bitmap {
                width: buffer.width(),
                height: buffer.height(),
                rgba: buffer.into_raw(),
            },
            delay_ms,
        });
    }
    match out.len() {
        0 => Err(DocumentError::Damaged),
        // A single frame is a still picture; decode it the usual way, which
        // also scales it.
        1 => Ok(None),
        _ => Ok(Some(out)),
    }
}

/// Renders an SVG drawing with its longer side at [`SVG_SIDE`] pixels.
fn drawing(bytes: &[u8]) -> Result<Bitmap, DocumentError> {
    use resvg::{tiny_skia, usvg};

    let mut options = usvg::Options {
        // Images inside the drawing must be embedded (data: URLs). A path
        // would make the preview read whatever the drawing names, such as
        // /dev/zero or another of the user's files.
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
    options.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|_| DocumentError::Damaged)?;
    let size = tree.size();
    let longer = size.width().max(size.height());
    if !longer.is_finite() || longer <= 0.0 {
        return Err(DocumentError::Damaged);
    }
    let scale = SVG_SIDE / longer;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "positive and at most SVG_SIDE"
    )]
    let pixels = |length: f32| ((length * scale).ceil() as u32).clamp(1, SVG_SIDE as u32);
    let mut pixmap = tiny_skia::Pixmap::new(pixels(size.width()), pixels(size.height()))
        .ok_or(DocumentError::Damaged)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let (width, height) = (pixmap.width(), pixmap.height());
    // tiny-skia keeps premultiplied alpha; bitmaps are straight RGBA.
    let mut rgba = pixmap.take();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha != 0 && alpha != 255 {
            for channel in &mut pixel[..3] {
                let straight = u16::from(*channel) * 255 / alpha;
                *channel = u8::try_from(straight.min(255)).unwrap_or(u8::MAX);
            }
        }
    }
    Ok(Bitmap {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// The CRC-32 PNG chunks carry.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn decodes_and_scales_raster_images() {
        let picture = decode(&png(3, 2), false).unwrap();
        assert_eq!(picture.frames.len(), 1);
        let bitmap = &picture.frames[0].bitmap;
        assert_eq!((bitmap.width, bitmap.height), (3, 2));
        assert_eq!(&bitmap.rgba[..4], [10, 20, 30, 255]);

        let picture = decode(&png(MAX_SIDE * 2, 4), false).unwrap();
        let bitmap = &picture.frames[0].bitmap;
        assert_eq!(bitmap.width, MAX_SIDE);
        assert_eq!(
            bitmap.rgba.len(),
            bitmap.width as usize * bitmap.height as usize * 4
        );
    }

    #[test]
    fn refuses_damaged_and_oversized_images() {
        assert_eq!(decode(b"not an image", false), Err(DocumentError::Damaged));
        // A PNG header claiming 100,000 × 100,000 pixels, with a valid
        // checksum, so only its size gives it away.
        let mut huge = png(1, 1);
        huge[16..20].copy_from_slice(&100_000_u32.to_be_bytes());
        huge[20..24].copy_from_slice(&100_000_u32.to_be_bytes());
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(decode(&huge, false), Err(DocumentError::TooLarge));
    }

    #[test]
    fn renders_svg_without_reading_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.png");
        std::fs::write(&outside, png(4, 4)).unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="20" height="10">
                 <rect width="10" height="10" fill="red"/>
                 <image x="10" width="10" height="10" xlink:href="{}"/>
               </svg>"#,
            outside.display()
        );
        let picture = decode(svg.as_bytes(), true).unwrap();
        let bitmap = &picture.frames[0].bitmap;
        assert_eq!((bitmap.width, bitmap.height), (2048, 1024));
        let pixel = |x: usize, y: usize| &bitmap.rgba[(y * 2048 + x) * 4..][..4];
        assert_eq!(pixel(10, 10), [255, 0, 0, 255]);
        // Where the file would have been drawn, nothing is.
        assert_eq!(pixel(1500, 500)[3], 0);
    }
}
