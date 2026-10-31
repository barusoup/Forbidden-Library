//! Bounded image decoding.
//!
//! Imported files are untrusted input: a header can claim dimensions that
//! would need gigabytes to decode, and a small compressed file can expand far
//! past the size of the file itself. Every full decode therefore goes through
//! these limits instead of `image::open`, which has no ceiling by default.
//!
//! The numbers match what the rest of the engine already accepts: a 32k side
//! and 150 megapixels cover camera and phone output (including panoramas),
//! and 512 MiB is below the point where a thumbnail pass could take the app
//! down on a machine that still has a working desktop.

use crate::error::{Error, Result};
use image::{DynamicImage, ImageReader};
use std::path::Path;

/// Largest width or height a decoder may accept.
pub const MAX_SIDE: u32 = 32_768;
/// Largest decoded pixel count.
pub const MAX_PIXELS: u64 = 150_000_000;
/// Hard ceiling on what one decoder may allocate.
pub const MAX_ALLOC: u64 = 512 * 1024 * 1024;

/// The limits every decode uses.
pub fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}

/// Decode a file, guessing the format from its signature rather than trusting
/// its extension. Used for originals, previews and optimizer candidates.
///
/// JPEG XL is decoded by jxl-oxide, because the `image` crate has no decoder
/// for it. Its pictures come back with the codestream's rotation applied; see
/// [`crate::jxl::oriented_by_decoder`].
pub fn open(path: &Path) -> Result<DynamicImage> {
    if crate::jxl::has_signature(path) {
        return crate::jxl::decode(path);
    }
    let mut reader = ImageReader::open(path)
        .map_err(|error| Error::Msg(format!("open image {}: {error}", path.display())))?
        .with_guessed_format()
        .map_err(|error| Error::Msg(format!("identify image {}: {error}", path.display())))?;
    reader.limits(limits());
    let image = reader
        .decode()
        .map_err(|error| Error::Msg(format!("decode image {}: {error}", path.display())))?;
    // The allocation limit already rejects most of these, but a format whose
    // decoder streams its rows can still report a size past the cap.
    check_dimensions(path, image.width(), image.height())?;
    Ok(image)
}

/// Refuse a picture with more pixels than [`MAX_PIXELS`]. Decoders that can
/// report their size first are checked before any pixel is produced.
pub(crate) fn check_dimensions(path: &Path, width: u32, height: u32) -> Result<()> {
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
        return Err(Error::Msg(format!(
            "image {} is {width}x{height}; larger than the {} megapixel limit",
            path.display(),
            MAX_PIXELS / 1_000_000
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_past_the_pixel_limit_is_refused_before_decoding() {
        let path = Path::new("huge.jxl");
        assert!(check_dimensions(path, 16_000, 9_000).is_ok());
        assert!(check_dimensions(path, 20_000, 20_000).is_err());
    }
}
