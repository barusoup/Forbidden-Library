use crate::error::{Error, Result};
use crate::ffmpeg::Tools;
use crate::model::Kind;
use crate::paths::Paths;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage};
use std::path::Path;

/// Longest edge of a cached preview. Tiles are usually 180-260 logical
/// pixels, so this keeps them sharp on HiDPI displays without making every
/// preview a full-size image.
pub const THUMB_SIDE: u32 = 384;
/// Longest edge of the on-disk viewer proxy. It is generated alongside the
/// grid thumb so opening an image never has to decode the archived original.
pub const VIEWER_SIDE: u32 = 2048;
/// Longest edge the full-resolution viewer ever decodes to. Larger originals
/// are reduced to this, which keeps a zoomed-in still within a GPU texture
/// limit most machines share.
pub const FULL_SIDE: u32 = 8192;

/// Version of the preview recipe, including the grid thumbnail and viewer
/// proxy. A cached preview built by an older recipe is rebuilt on next use.
/// Bump this whenever the output of [`generate`] changes.
pub const RECIPE: i64 = 3;

/// Quality of the cached preview JPEG. Visually lossless at the sizes the
/// grid draws, and small enough that decoding a tile is a fraction of a
/// millisecond.
const THUMB_QUALITY: u8 = 82;
/// Quality of the viewer proxy when it is stored as JPEG. Higher than the
/// grid's, because the viewer shows detail the grid never does.
const VIEWER_QUALITY: u8 = 92;

/// Background used to flatten transparency into a JPEG preview. Neutral
/// enough to sit quietly in either the light or dark theme.
const MATTE: [u8; 3] = [0x20, 0x21, 0x24];

pub struct ThumbOutcome {
    pub avg_color: u32,
}

/// Render a preview next to the catalogue. `source` is the stored file.
/// `orientation` is the EXIF orientation of the original.
///
/// The image is decoded once. It is scaled while still un-rotated (a full
/// resolution rotation of a 24 MP photo would copy a hundred megabytes for
/// no visible gain) and the average colour is taken from the pixels that
/// are written, so no second decode is needed.
pub fn generate(
    paths: &Paths,
    item_id: &str,
    source: &Path,
    kind: Kind,
    orientation: i32,
    duration_ms: Option<i64>,
    tools: &Tools,
) -> Result<ThumbOutcome> {
    let dest = paths.thumb_path(item_id);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match kind {
        Kind::Video => {
            let at = duration_ms.unwrap_or(0).clamp(0, 2000) / 10;
            let viewer = paths.viewer_jpeg_path(item_id);
            if let Some(parent) = viewer.parent() {
                std::fs::create_dir_all(parent)?;
            }
            poster_with_fallback(tools, source, &viewer, VIEWER_SIDE, at)?;
            let preview = crate::decode::open(&viewer)?;
            let flat = flatten(fit(preview, 1, THUMB_SIDE, FilterType::Lanczos3));
            write_jpeg(&flat, &dest, THUMB_QUALITY)?;
            Ok(ThumbOutcome {
                avg_color: average_color(&flat),
            })
        }
        Kind::Audio => {
            // The cover is copied out as the picture it is stored as, then
            // decoded like any photo. Seeking ffmpeg to a frame would skip an
            // attached picture in Ogg, which has no timestamp to seek to. A file
            // without a cover fails here, and the caller keeps it without one.
            let cover = paths
                .tmp()
                .join(format!("cover_{}.bin", crate::time::stamp()));
            let extracted = crate::ffmpeg::extract_cover(tools, source, &cover);
            let decoded = extracted.and_then(|_| decode_with_image(&cover));
            let _ = std::fs::remove_file(&cover);
            let image = decoded?;
            let viewer = fit(image, 1, VIEWER_SIDE, FilterType::Lanczos3);
            write_viewer(paths, item_id, &viewer)?;
            let flat = flatten(fit(viewer, 1, THUMB_SIDE, FilterType::Lanczos3));
            write_jpeg(&flat, &dest, THUMB_QUALITY)?;
            Ok(ThumbOutcome {
                avg_color: average_color(&flat),
            })
        }
        _ => {
            // ffmpeg frames are already rotated, so they are used as they
            // come. Everything the Rust decoders read is turned here.
            let (image, orientation) = match decode_with_image(source) {
                Ok(image) => (image, orientation_after_decode(source, orientation)),
                Err(image_error) => {
                    // HEIC, AVIF, RAW and friends. ffmpeg's lossless still is
                    // decoded like any other image, so the proxy below is made
                    // the same way as every other viewer proxy.
                    if !tools.has_ffmpeg() {
                        return Err(Error::Msg(format!(
                            "{image_error}; ffmpeg is required to preview this format"
                        )));
                    }
                    let still = ffmpeg_still(paths, tools, source, VIEWER_SIDE).map_err(
                        |ffmpeg_error| Error::Msg(format!("{image_error}; {ffmpeg_error}")),
                    )?;
                    (still, 1)
                }
            };
            let viewer = fit(image, orientation, VIEWER_SIDE, FilterType::Lanczos3);
            write_viewer(paths, item_id, &viewer)?;
            let flat = flatten(fit(viewer, 1, THUMB_SIDE, FilterType::Lanczos3));
            let avg_color = average_color(&flat);
            write_jpeg(&flat, &dest, THUMB_QUALITY)?;
            Ok(ThumbOutcome { avg_color })
        }
    }
}

fn decode_with_image(path: &Path) -> Result<DynamicImage> {
    crate::decode::open(path)
}

/// Decode a still the Rust decoders cannot read, through ffmpeg. The PNG
/// ffmpeg writes is staged in the library's `tmp/` and removed once decoded.
/// An opaque picture comes back as RGB, so it is stored as a JPEG proxy
/// rather than a PNG one.
fn ffmpeg_still(
    paths: &Paths,
    tools: &Tools,
    source: &Path,
    max_side: u32,
) -> Result<DynamicImage> {
    let temp = paths
        .tmp()
        .join(format!("still_{}.png", crate::time::stamp()));
    let decoded = crate::ffmpeg::image_poster(tools, source, &temp, max_side)
        .and_then(|_| decode_with_image(&temp));
    let _ = std::fs::remove_file(&temp);
    decoded.map(without_opaque_alpha)
}

/// Drop an alpha channel that holds only opaque pixels. ffmpeg always writes
/// RGBA, so most stills arrive with one even when nothing is transparent.
fn without_opaque_alpha(image: DynamicImage) -> DynamicImage {
    let opaque = image
        .as_rgba8()
        .is_some_and(|rgba| rgba.pixels().all(|pixel| pixel[3] == u8::MAX));
    if opaque {
        DynamicImage::ImageRgb8(image.to_rgb8())
    } else {
        image
    }
}

/// The orientation still to apply after decoding `source`. A JPEG XL whose
/// codestream carries a rotation comes out of the decoder already turned, so
/// the EXIF orientation of the original must not be applied on top of it.
pub(crate) fn orientation_after_decode(source: &Path, orientation: i32) -> i32 {
    if crate::jxl::oriented_by_decoder(source) {
        1
    } else {
        orientation
    }
}

/// Decode the persistent viewer proxy for an item, creating it from the
/// original once when the archive predates this cache.
#[allow(clippy::too_many_arguments)]
pub fn preview_rgba(
    paths: &Paths,
    item_id: &str,
    source: &Path,
    kind: Kind,
    orientation: i32,
    duration_ms: Option<i64>,
    max_side: u32,
    tools: Option<&Tools>,
) -> Result<(u32, u32, Vec<u8>)> {
    let jpeg = paths.viewer_jpeg_path(item_id);
    let png = paths.viewer_png_path(item_id);
    let cached_path = if png.is_file() {
        Some(png.as_path())
    } else if jpeg.is_file() {
        Some(jpeg.as_path())
    } else {
        None
    };

    let image = if let Some(path) = cached_path {
        decode_with_image(path)?
    } else {
        let fitted = preview_rgba_uncached(
            paths,
            source,
            kind,
            orientation,
            duration_ms,
            VIEWER_SIDE,
            tools,
            FilterType::Lanczos3,
        )?;
        write_viewer(paths, item_id, &fitted)?;
        fitted
    };
    let fitted = fit(image, 1, max_side.min(VIEWER_SIDE), FilterType::Lanczos3);
    let rgba = fitted.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((width, height, rgba.into_raw()))
}

/// Decode the full-resolution frame of an item for close inspection. Unlike
/// [`preview_rgba`] this never reads the cached proxy: it works from the
/// original, reduced only as far as `max_side` (capped at [`FULL_SIDE`]) asks.
/// `max_side` of zero means the largest size allowed.
///
/// The reduction uses Triangle rather than Lanczos3. A full-size photo is a
/// large downscale, and Lanczos3 there is too slow for a viewer waiting on it.
pub fn full_rgba(
    paths: &Paths,
    source: &Path,
    kind: Kind,
    orientation: i32,
    duration_ms: Option<i64>,
    max_side: u32,
    tools: Option<&Tools>,
) -> Result<(u32, u32, Vec<u8>)> {
    let side = if max_side == 0 {
        FULL_SIDE
    } else {
        max_side.min(FULL_SIDE)
    };
    let image = preview_rgba_uncached(
        paths,
        source,
        kind,
        orientation,
        duration_ms,
        side,
        tools,
        FilterType::Triangle,
    )?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((width, height, rgba.into_raw()))
}

#[allow(clippy::too_many_arguments)]
fn preview_rgba_uncached(
    paths: &Paths,
    source: &Path,
    kind: Kind,
    orientation: i32,
    duration_ms: Option<i64>,
    max_side: u32,
    tools: Option<&Tools>,
    filter: FilterType,
) -> Result<DynamicImage> {
    // ffmpeg applies the container/EXIF rotation itself, so its frames must
    // not be turned a second time.
    let decoded = if kind != Kind::Video {
        decode_with_image(source)
    } else {
        Err(Error::Msg("video requires ffmpeg".into()))
    };
    match decoded {
        Ok(image) => Ok(fit(
            image,
            orientation_after_decode(source, orientation),
            max_side,
            filter,
        )),
        Err(_) => {
            let tools = tools
                .ok_or_else(|| Error::Msg("ffmpeg is required to preview this file".into()))?;
            if !tools.has_ffmpeg() {
                return Err(Error::Msg("ffmpeg is required to preview this file".into()));
            }
            if kind != Kind::Video {
                return ffmpeg_still(paths, tools, source, max_side);
            }
            let temp = paths
                .tmp()
                .join(format!("preview_{}.jpg", crate::time::stamp()));
            let at = duration_ms.unwrap_or(0).clamp(0, 2000) / 10;
            let decoded = poster_with_fallback(tools, source, &temp, max_side, at)
                .and_then(|_| decode_with_image(&temp));
            let _ = std::fs::remove_file(&temp);
            decoded
        }
    }
}

/// A video poster a little way in, which skips black lead-in frames. When
/// that offset has nothing decodable (a one-frame clip, or a long-GOP stream
/// whose seek lands past its only keyframe), the very first frame is used.
fn poster_with_fallback(
    tools: &Tools,
    source: &Path,
    dest: &Path,
    max_side: u32,
    at_ms: i64,
) -> Result<()> {
    match crate::ffmpeg::video_poster(tools, source, dest, max_side, at_ms) {
        Err(_) if at_ms > 0 => crate::ffmpeg::video_poster(tools, source, dest, max_side, 0),
        result => result,
    }
}

fn write_viewer(paths: &Paths, item_id: &str, image: &DynamicImage) -> Result<()> {
    let jpeg = paths.viewer_jpeg_path(item_id);
    let png = paths.viewer_png_path(item_id);
    if let Some(parent) = jpeg.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if image.color().has_alpha() {
        image.save_with_format(&png, image::ImageFormat::Png)?;
        let _ = std::fs::remove_file(jpeg);
    } else {
        write_jpeg(&image.to_rgb8(), &jpeg, VIEWER_QUALITY)?;
        let _ = std::fs::remove_file(png);
    }
    Ok(())
}

/// Scale an image to fit `max_side` and then apply its EXIF orientation.
///
/// Scaling before rotating is the whole point of this function: rotating a
/// full-size photo costs a full-size copy, while rotating the tile-sized
/// result is free. The box is swapped for the quarter turns so the final
/// image still fits the box.
fn fit(image: DynamicImage, orientation: i32, max_side: u32, filter: FilterType) -> DynamicImage {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return image;
    }
    let quarter_turn = matches!(orientation, 5..=8);
    let (box_w, box_h) = if quarter_turn {
        (height, width)
    } else {
        (width, height)
    };
    let (target_w, target_h) = fit_dims(box_w, box_h, max_side);
    let (scale_w, scale_h) = if quarter_turn {
        // The pre-rotation image must be the transpose of the target.
        (target_h, target_w)
    } else {
        (target_w, target_h)
    };
    let scaled = resize(image, scale_w, scale_h, filter);
    crate::thumb::orient(scaled, orientation)
}

/// Target size inside a `max_side` box, never upscaling and never zero.
pub fn fit_dims(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let longest = width.max(height).max(1);
    if longest <= max_side {
        return (width.max(1), height.max(1));
    }
    let scale = max_side as f64 / longest as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

/// Resize with a box pre-pass for large reductions.
///
/// Lanczos3 is expensive when the source is thousands of pixels wide, and
/// most of its taps then land on information an area average would have
/// already folded together. Shrinking to twice the target with the integer
/// box filter and finishing with one Lanczos3 pass looks the same and is
/// several times faster on a large photo.
pub fn resize(image: DynamicImage, width: u32, height: u32, filter: FilterType) -> DynamicImage {
    let (source_w, source_h) = image.dimensions();
    let (width, height) = (width.max(1), height.max(1));
    if source_w == width && source_h == height {
        return image;
    }
    if source_w >= width.saturating_mul(2) && source_h >= height.saturating_mul(2) {
        let pre_w = (width * 2).min(source_w);
        let pre_h = (height * 2).min(source_h);
        return image
            .thumbnail(pre_w, pre_h)
            .resize_exact(width, height, filter);
    }
    image.resize_exact(width, height, filter)
}

/// Composite any alpha channel onto the matte and hand back RGB. Images
/// without transparency (the common case: every JPEG) skip the per-pixel
/// blend entirely.
fn flatten(image: DynamicImage) -> RgbImage {
    if image.color().has_alpha() {
        blend_over_matte(&image.to_rgba8())
    } else {
        image.to_rgb8()
    }
}

fn blend_over_matte(image: &RgbaImage) -> RgbImage {
    let (width, height) = image.dimensions();
    let mut out = Vec::with_capacity(width as usize * height as usize * 3);
    for pixel in image.as_raw().chunks_exact(4) {
        let alpha = pixel[3] as u32;
        match alpha {
            255 => out.extend_from_slice(&pixel[..3]),
            0 => out.extend_from_slice(&MATTE),
            _ => {
                let inverse = 255 - alpha;
                for channel in 0..3 {
                    let value =
                        (pixel[channel] as u32 * alpha + MATTE[channel] as u32 * inverse + 127)
                            / 255;
                    out.push(value as u8);
                }
            }
        }
    }
    RgbImage::from_raw(width, height, out).unwrap_or_else(|| RgbImage::new(width, height))
}

fn write_jpeg(image: &RgbImage, dest: &Path, quality: u8) -> Result<()> {
    let file = std::fs::File::create(dest)
        .map_err(|e| Error::Msg(format!("create preview {}: {e}", dest.display())))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(writer, quality);
    let (width, height) = image.dimensions();
    encoder
        .encode(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| Error::Msg(format!("encode preview: {e}")))
}

/// Average colour of a preview, sampled on a bounded grid. The tile only
/// needs a representative colour, so a stride keeps this at a few thousand
/// pixels no matter how the preview size changes.
fn average_color(image: &RgbImage) -> u32 {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return 0x2b2b2b;
    }
    let step_x = (width / 64).max(1) as usize;
    let step_y = (height / 64).max(1) as usize;
    let row = width as usize * 3;
    let raw = image.as_raw();
    let mut sums = [0u64; 3];
    let mut samples = 0u64;
    for y in (0..height as usize).step_by(step_y) {
        let line = &raw[y * row..y * row + row];
        for x in (0..width as usize).step_by(step_x) {
            let pixel = &line[x * 3..x * 3 + 3];
            sums[0] += pixel[0] as u64;
            sums[1] += pixel[1] as u64;
            sums[2] += pixel[2] as u64;
        }
        samples += 1;
    }
    let samples = samples * ((width as usize).div_ceil(step_x)) as u64;
    if samples == 0 {
        return 0x2b2b2b;
    }
    let channel = |index: usize| -> u32 { ((sums[index] / samples) as u32).min(255) };
    (channel(0) << 16) | (channel(1) << 8) | channel(2)
}

/// Apply the EXIF orientation so previews are upright.
pub fn orient(image: DynamicImage, orientation: i32) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.fliph().rotate270(),
        6 => image.rotate90(),
        7 => image.fliph().rotate90(),
        8 => image.rotate270(),
        _ => image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, Rgba};

    #[test]
    fn average_color_of_a_solid_image_is_that_color() {
        let image = RgbImage::from_pixel(16, 16, Rgb([10, 120, 240]));
        assert_eq!(average_color(&image), 0x0a78f0);
    }

    #[test]
    fn flatten_composites_transparency_on_the_matte() {
        let image = RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 0]));
        assert_eq!(blend_over_matte(&image).get_pixel(0, 0).0, MATTE);
    }

    #[test]
    fn flatten_keeps_opaque_images_untouched() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, Rgb([1, 2, 3])));
        assert_eq!(flatten(image).get_pixel(0, 0).0, [1, 2, 3]);
    }

    #[test]
    fn an_opaque_still_drops_its_alpha_but_a_transparent_one_keeps_it() {
        let opaque = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([9, 8, 7, 255])));
        assert!(matches!(
            without_opaque_alpha(opaque),
            DynamicImage::ImageRgb8(_)
        ));
        let cutout = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([9, 8, 7, 0])));
        assert!(matches!(
            without_opaque_alpha(cutout),
            DynamicImage::ImageRgba8(_)
        ));
    }

    #[test]
    fn fit_dims_never_upscales_and_keeps_aspect() {
        assert_eq!(fit_dims(4000, 3000, THUMB_SIDE), (384, 288));
        assert_eq!(fit_dims(100, 50, THUMB_SIDE), (100, 50));
        assert_eq!(fit_dims(1, 10_000, THUMB_SIDE), (1, 384));
    }

    #[test]
    fn a_quarter_turn_is_applied_after_scaling() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(400, 200, Rgb([9, 9, 9])));
        let turned = fit(image, 6, 100, FilterType::Lanczos3);
        // 400x200 fits a 100 box as 100x50; after a 90 degree turn it is 50x100.
        assert_eq!(turned.dimensions(), (50, 100));
    }

    #[test]
    fn resizing_large_images_uses_the_same_target_size() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(1000, 500, Rgb([4, 5, 6])));
        let small = resize(image, 250, 125, FilterType::Lanczos3);
        assert_eq!(small.dimensions(), (250, 125));
        assert_eq!(small.get_pixel(10, 10).0, [4, 5, 6, 255]);
    }
}
