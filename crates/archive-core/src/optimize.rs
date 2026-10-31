//! Lossless image optimization at import.
//!
//! An image is rewritten while it is still in staging, before it enters the
//! content-addressed store, so the library only ever holds the final form.
//! Every candidate reproduces the original exactly, and nothing lossy is ever
//! written:
//!
//! - A JPEG becomes JPEG XL by lossless transcoding. The candidate is kept
//!   only if djxl rebuilds the original file byte for byte. No pixels are
//!   decoded, so there is no visual comparison to get wrong.
//! - A PNG or a plain BMP becomes a smaller PNG, a lossless JPEG XL, or (for
//!   8-bit PNGs without metadata) a lossless WebP. Each candidate is decoded
//!   and compared sample for sample, at the precision of the source.
//!
//! TIFF is left alone. The converter cannot carry its tags (EXIF, ICC and
//! XMP), so a conversion would drop metadata.
//!
//! A failure at any step leaves the original in place. The order is fixed:
//!
//! 1. produce candidates in a scratch folder,
//! 2. verify the smallest first, and stop at the first that passes,
//! 3. the caller moves the winning candidate into the store and commits the
//!    catalogue row.
//!
//! The replaced original is not copied aside, so the rewrite is final.

use crate::error::{Error, Result};
use crate::ffmpeg::Tools;
use crate::jxl;
use crate::model::Kind;
use crate::store;
use crate::time;
use image::{ColorType, DynamicImage, GenericImageView, ImageEncoder};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct OptimizeConfig {
    /// How much smaller a candidate must be, as a percentage of the original.
    pub min_savings_percent: f64,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            min_savings_percent: 2.0,
        }
    }
}

/// Files this small gain too little from a rewrite to be worth the decode.
const MIN_OPTIMIZE_BYTES: u64 = 16 * 1024;

/// A verified, smaller encoding of a staged image. The file sits in the
/// staging folder until the caller moves it into the store.
pub(crate) struct Replacement {
    pub path: PathBuf,
    pub ext: String,
    pub mime: String,
    pub kind: &'static str,
}

/// How a candidate is described once it is stored.
#[derive(Clone, Copy)]
struct Format {
    ext: &'static str,
    mime: &'static str,
    kind: &'static str,
}

const PNG_RECOMPRESSED: Format = Format {
    ext: "png",
    mime: "image/png",
    kind: "PNG recompression",
};
const PNG_CONVERTED: Format = Format {
    ext: "png",
    mime: "image/png",
    kind: "PNG conversion",
};
const WEBP_LOSSLESS: Format = Format {
    ext: "webp",
    mime: "image/webp",
    kind: "Lossless WebP",
};
const JXL_LOSSLESS: Format = Format {
    ext: "jxl",
    mime: "image/jxl",
    kind: "JPEG XL (lossless)",
};
const JXL_REVERSIBLE: Format = Format {
    ext: "jxl",
    mime: "image/jxl",
    kind: "JPEG XL (reversible)",
};

/// One encoding waiting to be verified.
struct Candidate {
    path: PathBuf,
    format: Format,
}

/// Whether an imported file is worth the optimizer's time. `ext` is lower case.
pub(crate) fn is_candidate(kind: Kind, ext: &str, bytes: u64) -> bool {
    kind == Kind::Image
        && bytes > MIN_OPTIMIZE_BYTES
        && matches!(ext, "png" | "jpg" | "jpeg" | "bmp")
}

/// Finds the smallest encoding of `source` that verifiably reproduces it.
/// Candidates are written under `tmp`. The winner is returned and every other
/// candidate is removed. `None` means nothing was both smaller and verified,
/// so the original stays as it is.
pub(crate) fn best_replacement(
    source: &Path,
    ext: &str,
    tmp: &Path,
    tools: &Tools,
    config: &OptimizeConfig,
) -> Result<Option<Replacement>> {
    // Still-image pixel comparisons cannot verify animation frames or timing.
    // Never replace an animation with a verified copy of just its first frame.
    if crate::anim::is_animated(source) {
        return Ok(None);
    }
    // The lightweight animation sniffer reads a bounded header. A PNG may
    // place large ancillary chunks before acTL, so consult the decoder too.
    let head = read_head(source).unwrap_or_default();
    if head.starts_with(PNG_SIGNATURE) {
        let decoder = image::codecs::png::PngDecoder::with_limits(
            std::io::BufReader::new(std::fs::File::open(source)?),
            crate::decode::limits(),
        )?;
        if decoder.is_apng()? {
            return Ok(None);
        }
    }
    let original_size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);

    let mut scratch = Scratch::new(tmp);
    let winner = match ext {
        "jpg" | "jpeg" => {
            let candidates = jpeg_candidates(source, &mut scratch, tools);
            smallest_verified(candidates, original_size, config, |candidate| {
                rebuilds_original(source, &candidate.path, &mut scratch, tools)
            })
        }
        "png" | "bmp" => {
            let decoded = crate::decode::open(source)
                .map_err(|error| Error::Msg(format!("decode {}: {error}", source.display())))?;
            let candidates = pixel_candidates(source, ext, &head, &decoded, &mut scratch, tools);
            smallest_verified(candidates, original_size, config, |candidate| {
                decodes_to_same_samples(&decoded, &candidate.path)
            })
        }
        _ => None,
    };
    Ok(winner.map(|candidate| {
        scratch.keep(&candidate.path);
        Replacement {
            path: candidate.path,
            ext: candidate.format.ext.into(),
            mime: candidate.format.mime.into(),
            kind: candidate.format.kind,
        }
    }))
}

/// The candidates for a JPEG. A JPEG is only ever rewritten as JPEG XL, and
/// only when the tools that can check the result are both present.
fn jpeg_candidates(source: &Path, scratch: &mut Scratch, tools: &Tools) -> Vec<Candidate> {
    if !tools.has_jxl() {
        return Vec::new();
    }
    let path = scratch.file("jxl", "jxl");
    match jxl::transcode_jpeg(tools, source, &path) {
        Ok(()) => vec![Candidate {
            path,
            format: JXL_REVERSIBLE,
        }],
        Err(_) => Vec::new(),
    }
}

/// The candidates for a PNG or a plain BMP. Each one is verified against
/// `decoded`, so a candidate has to reproduce every sample.
fn pixel_candidates(
    source: &Path,
    ext: &str,
    head: &[u8],
    decoded: &DynamicImage,
    scratch: &mut Scratch,
    tools: &Tools,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    match ext {
        "png" => {
            if let Some(path) = optimize_png(source, scratch) {
                out.push(candidate(path, PNG_RECOMPRESSED));
            }
            // Text chunks may follow the image data, so the whole file's chunk
            // list is read, not just the head. A file that cannot be walked is
            // treated as holding metadata.
            let chunks = png_file_chunks(source);
            // cjxl carries ICC profiles and EXIF, but it drops text chunks.
            if chunks.as_deref().is_some_and(|kinds| !png_has_text(kinds)) {
                out.extend(lossless_jxl(source, scratch, tools));
            }
            // WebP keeps neither profiles nor EXIF, and cannot hold more than
            // eight bits a channel, so it is offered only when it is lossless
            // for the whole file.
            if chunks
                .as_deref()
                .is_some_and(|kinds| !png_has_metadata(kinds))
                && sample_bits(decoded.color()) == 8
                && let Some(path) = encode_lossless_webp(decoded, scratch)
            {
                out.push(candidate(path, WEBP_LOSSLESS));
            }
        }
        "bmp" if bmp_is_plain(head) => {
            // cjxl cannot read BMP, so both candidates start from a lossless
            // PNG of the same samples.
            if let Some(png) = encode_png(decoded, scratch) {
                out.extend(lossless_jxl(&png, scratch, tools));
                out.push(candidate(png, PNG_CONVERTED));
            }
        }
        _ => {}
    }
    out
}

fn candidate(path: PathBuf, format: Format) -> Candidate {
    Candidate { path, format }
}

fn lossless_jxl(png: &Path, scratch: &mut Scratch, tools: &Tools) -> Option<Candidate> {
    if !tools.has_jxl() {
        return None;
    }
    let path = scratch.file("jxl", "jxl");
    jxl::encode_lossless(tools, png, &path).ok()?;
    Some(candidate(path, JXL_LOSSLESS))
}

/// The candidate that is both smaller than the original by the configured
/// margin and passes `verify`. Candidates are tried smallest first, so a
/// passing candidate usually costs one verification.
fn smallest_verified(
    candidates: Vec<Candidate>,
    original_size: u64,
    config: &OptimizeConfig,
    mut verify: impl FnMut(&Candidate) -> bool,
) -> Option<Candidate> {
    let mut sized: Vec<(u64, Candidate)> = candidates
        .into_iter()
        .filter_map(|candidate| Some((saving(&candidate.path, original_size, config)?, candidate)))
        .collect();
    sized.sort_by_key(|(size, _)| *size);
    sized
        .into_iter()
        .map(|(_, candidate)| candidate)
        .find(|candidate| verify(candidate))
}

/// The size of `path` if it is a real file that beats the original by the
/// configured margin. Anything else is `None`.
fn saving(path: &Path, original_size: u64, config: &OptimizeConfig) -> Option<u64> {
    let size = std::fs::metadata(path).ok()?.len();
    if size == 0 || size >= original_size {
        return None;
    }
    let ratio = (original_size - size) as f64 / original_size.max(1) as f64 * 100.0;
    (ratio >= config.min_savings_percent).then_some(size)
}

/// Whether djxl rebuilds `candidate` into the original file, byte for byte.
/// The hashes cover the whole file, so one changed byte anywhere rejects it.
fn rebuilds_original(
    original: &Path,
    candidate: &Path,
    scratch: &mut Scratch,
    tools: &Tools,
) -> bool {
    let rebuilt = scratch.file("rebuilt", "jpg");
    if jxl::reconstruct_jpeg(tools, candidate, &rebuilt).is_err() {
        return false;
    }
    match (store::hash_file(original), store::hash_file(&rebuilt)) {
        (Ok(expected), Ok(actual)) => expected == actual,
        _ => false,
    }
}

/// Whether the candidate decodes to exactly the samples of `decoded`.
fn decodes_to_same_samples(decoded: &DynamicImage, candidate: &Path) -> bool {
    crate::decode::open(candidate).is_ok_and(|image| same_samples(decoded, &image))
}

/// Whether two decodes hold the same samples at the same precision.
///
/// The comparison stays at the precision of the source. Widening both sides
/// to 16 bits first would call two 16-bit images equal when they differ only
/// in their low byte, which an 8-bit candidate would then silently drop.
fn same_samples(original: &DynamicImage, candidate: &DynamicImage) -> bool {
    if original.dimensions() != candidate.dimensions() {
        return false;
    }
    match (
        sample_bits(original.color()),
        sample_bits(candidate.color()),
    ) {
        (8, 8) => original.to_rgba8().as_raw() == candidate.to_rgba8().as_raw(),
        (16, 16) => original.to_rgba16().as_raw() == candidate.to_rgba16().as_raw(),
        // No candidate is ever floating point, so no other depth is accepted.
        _ => false,
    }
}

/// Bits per colour sample: 8 for `L8`/`Rgba8`, 16 for `Rgb16`, and so on.
fn sample_bits(color: ColorType) -> u16 {
    color.bits_per_pixel() / u16::from(color.channel_count())
}

/// Every scratch file lives in the library's own `tmp/`, never the system temp
/// folder: the final move must not cross a volume, and opening the library
/// wipes anything a crash left behind.
///
/// Everything handed out is removed when the pass ends, except the file that
/// was [kept](Scratch::keep) as the winner. The extension is part of each name
/// because the encoders pick their output format from it.
struct Scratch {
    dir: PathBuf,
    files: Vec<PathBuf>,
}

impl Scratch {
    fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            files: Vec::new(),
        }
    }

    fn file(&mut self, label: &str, ext: &str) -> PathBuf {
        let path = self
            .dir
            .join(format!("opt_{label}_{}.{ext}", time::stamp()));
        self.files.push(path.clone());
        path
    }

    fn keep(&mut self, path: &Path) {
        self.files.retain(|file| file != path);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.files {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn optimize_png(source: &Path, scratch: &mut Scratch) -> Option<PathBuf> {
    let dest = scratch.file("png", "png");
    let options = oxipng::Options {
        strip: oxipng::StripChunks::None,
        // Skips the most expensive trial encodes. The pixel check is what
        // decides whether the result is kept, not the encoder.
        fast_evaluation: true,
        ..oxipng::Options::default()
    };
    oxipng::optimize(
        &oxipng::InFile::Path(source.to_path_buf()),
        &oxipng::OutFile::Path {
            path: Some(dest.clone()),
            preserve_attrs: true,
        },
        &options,
    )
    .ok()?;
    dest.is_file().then_some(dest)
}

fn encode_lossless_webp(decoded: &DynamicImage, scratch: &mut Scratch) -> Option<PathBuf> {
    let dest = scratch.file("pngwebp", "webp");
    let rgba = decoded.to_rgba8();
    let file = std::fs::File::create(&dest).ok()?;
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(std::io::BufWriter::new(file));
    let (width, height) = rgba.dimensions();
    encoder
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(dest)
}

fn encode_png(image: &DynamicImage, scratch: &mut Scratch) -> Option<PathBuf> {
    let dest = scratch.file("bmp", "png");
    image
        .save_with_format(&dest, image::ImageFormat::Png)
        .ok()?;
    dest.is_file().then_some(dest)
}

const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// Enough of the file to hold every metadata chunk a camera or editor writes
/// before the pixels. Reading the whole image just to look for a profile was
/// costing a full extra pass over the file.
const HEAD_BYTES: u64 = 256 * 1024;

fn read_head(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::with_capacity(4096);
    file.take(HEAD_BYTES).read_to_end(&mut head).ok()?;
    Some(head)
}

/// The type of every chunk in the PNG at `path`, up to `IEND`. Only the
/// eight-byte chunk headers are read; the data in between is skipped, so a
/// large image costs a handful of small reads. `None` when the file is not a
/// whole, readable PNG.
fn png_file_chunks(path: &Path) -> Option<Vec<[u8; 4]>> {
    png_chunks(std::io::BufReader::new(std::fs::File::open(path).ok()?))
}

fn png_chunks(mut reader: impl Read + std::io::Seek) -> Option<Vec<[u8; 4]>> {
    let mut signature = [0u8; 8];
    reader.read_exact(&mut signature).ok()?;
    if signature != PNG_SIGNATURE {
        return None;
    }
    let mut kinds = Vec::new();
    loop {
        let mut header = [0u8; 8];
        reader.read_exact(&mut header).ok()?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let kind = [header[4], header[5], header[6], header[7]];
        kinds.push(kind);
        if &kind == b"IEND" {
            return Some(kinds);
        }
        // The chunk's data and its CRC.
        reader
            .seek(std::io::SeekFrom::Current(i64::from(length) + 4))
            .ok()?;
    }
}

/// Text chunks. cjxl does not carry these, so a PNG holding one cannot become
/// JPEG XL without losing it.
const PNG_TEXT_CHUNKS: [&[u8; 4]; 3] = [b"tEXt", b"zTXt", b"iTXt"];

fn png_has_text(kinds: &[[u8; 4]]) -> bool {
    kinds.iter().any(|kind| PNG_TEXT_CHUNKS.contains(&kind))
}

/// Any chunk a re-encode could drop: text, an ICC profile or EXIF.
fn png_has_metadata(kinds: &[[u8; 4]]) -> bool {
    kinds
        .iter()
        .any(|kind| PNG_TEXT_CHUNKS.contains(&kind) || kind == b"iCCP" || kind == b"eXIf")
}

/// A BMP whose header is the basic (or OS/2 core) kind. Newer headers can
/// embed a colour profile, which the converter would drop.
fn bmp_is_plain(head: &[u8]) -> bool {
    // The header size is a little-endian u32 straight after the 14-byte file
    // header. 12 is the OS/2 core header and 40 the basic one.
    head.starts_with(b"BM")
        && head.len() >= 18
        && matches!(
            u32::from_le_bytes([head[14], head[15], head[16], head[17]]),
            12 | 40
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbImage, RgbaImage};

    #[test]
    fn animations_disguised_as_stills_are_never_optimized() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("animation.png");
        {
            let mut encoder =
                image::codecs::gif::GifEncoder::new(std::fs::File::create(&source).unwrap());
            for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
                encoder
                    .encode_frame(image::Frame::new(RgbaImage::from_pixel(
                        64,
                        64,
                        image::Rgba(color),
                    )))
                    .unwrap();
            }
        }
        let before = std::fs::read(&source).unwrap();
        assert!(
            best_replacement(&source, "png", temp.path(), &no_tools(), &config())
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&source).unwrap(), before);
        assert_eq!(crate::anim::frames(&source, 64).unwrap().count(), 2);
    }

    fn config() -> OptimizeConfig {
        OptimizeConfig::default()
    }

    fn no_tools() -> Tools {
        Tools {
            ffmpeg: None,
            ffprobe: None,
            cjxl: None,
            djxl: None,
        }
    }

    /// A 16-bit picture whose high bytes vary across the frame and whose low
    /// byte is `low` everywhere. Two pictures made with different `low` values
    /// have the same 8-bit view and different 16-bit samples.
    fn sixteen_bit(width: u32, height: u32, low: u16) -> DynamicImage {
        let image = ImageBuffer::<Rgb<u16>, Vec<u16>>::from_fn(width, height, |x, y| {
            Rgb([
                (((x * 3) as u16) << 8) | low,
                (((y * 5) as u16) << 8) | low,
                0x7f00 | low,
            ])
        });
        DynamicImage::ImageRgb16(image)
    }

    #[test]
    fn a_picture_differing_only_in_low_bytes_is_not_the_same_at_its_own_precision() {
        let source = sixteen_bit(32, 32, 0x10);
        let differs_in_low_byte = sixteen_bit(32, 32, 0x20);
        assert!(same_samples(&source, &source));
        assert!(!same_samples(&source, &differs_in_low_byte));
        // The 8-bit view of both is identical, so an 8-bit candidate would
        // pass a comparison done at 8 bits. It must not pass this one.
        let eight_bit = DynamicImage::ImageRgb8(source.to_rgb8());
        assert!(!same_samples(&differs_in_low_byte, &eight_bit));
        assert!(!same_samples(&source, &eight_bit));
    }

    #[test]
    fn a_sixteen_bit_png_is_never_replaced_by_an_eight_bit_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("deep.png");
        sixteen_bit(256, 256, 0x10)
            .save_with_format(&source, image::ImageFormat::Png)
            .unwrap();
        let original = image::open(&source).unwrap();

        let replacement =
            best_replacement(&source, "png", temp.path(), &no_tools(), &config()).unwrap();
        if let Some(replacement) = replacement {
            assert_ne!(replacement.ext, "webp", "WebP is 8-bit only");
            let stored = crate::decode::open(&replacement.path).unwrap();
            assert_eq!(stored.color(), original.color());
            assert!(same_samples(&original, &stored));
        }
    }

    #[test]
    fn a_jpeg_without_the_jxl_tools_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("photo.jpg");
        let image = RgbImage::from_pixel(64, 64, Rgb([40, 80, 120]));
        image
            .save_with_format(&source, image::ImageFormat::Jpeg)
            .unwrap();
        let before = std::fs::read(&source).unwrap();
        assert!(
            best_replacement(&source, "jpg", temp.path(), &no_tools(), &config())
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }

    #[test]
    fn identical_pixels_are_the_same_at_every_depth_they_share() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 8, Rgb([12, 34, 56])));
        assert!(same_samples(&image, &image));
        let other = DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 8, Rgb([12, 34, 57])));
        assert!(!same_samples(&image, &other));
    }

    /// A PNG made of the given chunks, each with `length` zero bytes of data
    /// and a dummy CRC, ended by `IEND`.
    fn png_of(chunks: &[(&[u8; 4], u32)]) -> Vec<u8> {
        let mut png = PNG_SIGNATURE.to_vec();
        for (kind, length) in chunks.iter().copied().chain([(b"IEND", 0)]) {
            png.extend_from_slice(&length.to_be_bytes());
            png.extend_from_slice(kind);
            png.extend(std::iter::repeat_n(0u8, length as usize + 4));
        }
        png
    }

    fn chunks_of(png: Vec<u8>) -> Vec<[u8; 4]> {
        png_chunks(std::io::Cursor::new(png)).unwrap()
    }

    #[test]
    fn metadata_sniffing_reads_chunks_without_the_pixels() {
        let plain = chunks_of(png_of(&[(b"IHDR", 13), (b"IDAT", 64)]));
        assert!(!png_has_metadata(&plain));
        let profiled = chunks_of(png_of(&[(b"IHDR", 13), (b"iCCP", 32), (b"IDAT", 64)]));
        assert!(png_has_metadata(&profiled));
        // An ICC profile is carried by JPEG XL, so it is not text.
        assert!(!png_has_text(&profiled));
    }

    #[test]
    fn a_text_chunk_is_metadata_that_jpeg_xl_cannot_carry() {
        for kind in [b"tEXt", b"zTXt", b"iTXt"] {
            let kinds = chunks_of(png_of(&[(b"IHDR", 13), (kind, 8)]));
            assert!(png_has_text(&kinds), "{kind:?} not seen as text");
            assert!(png_has_metadata(&kinds));
        }
    }

    #[test]
    fn a_text_chunk_after_large_image_data_is_still_found() {
        // Text may follow the pixels, far past any fixed-size head.
        let kinds = chunks_of(png_of(&[(b"IHDR", 13), (b"IDAT", 4 << 20), (b"tEXt", 8)]));
        assert!(png_has_text(&kinds));
    }

    #[test]
    fn a_truncated_png_cannot_be_walked() {
        let mut png = png_of(&[(b"IHDR", 13), (b"IDAT", 64)]);
        png.truncate(png.len() - 20);
        assert!(png_chunks(std::io::Cursor::new(png)).is_none());
    }

    #[test]
    fn only_plain_bmp_headers_are_converted() {
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 12]);
        bmp.extend_from_slice(&40u32.to_le_bytes());
        assert!(bmp_is_plain(&bmp));
        // A version 5 header can carry an ICC profile.
        let mut v5 = bmp.clone();
        v5[14..18].copy_from_slice(&124u32.to_le_bytes());
        assert!(!bmp_is_plain(&v5));
    }
}
