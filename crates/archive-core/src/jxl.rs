//! JPEG XL, in two halves.
//!
//! Encoding and JPEG reconstruction run libjxl's `cjxl` and `djxl`, bundled
//! beside the app and bounded like every other media tool. Decoding is done
//! in-app by jxl-oxide, so opening a stored JPEG XL never depends on the
//! operating system or on an external program.
//!
//! The decoder applies the orientation carried by the codestream while it
//! renders. A caller that also applies the EXIF orientation of the original
//! must therefore ask [`oriented_by_decoder`] first, or it rotates the picture
//! twice.

use crate::decode;
use crate::error::{Error, Result};
use crate::ffmpeg::{self, Bounded, DEADLINE, Tools};
use image::{DynamicImage, ImageDecoder};
use jxl_oxide::JxlImage;
use jxl_oxide::integration::JxlDecoder;
use std::io::{BufReader, Read};
use std::path::Path;

/// A bare codestream begins with this pair.
const CODESTREAM_SIGNATURE: &[u8] = &[0xff, 0x0a];
/// A container begins with this box, then the same magic `JXL ` and
/// signature bytes as the codestream inside it.
const CONTAINER_SIGNATURE: &[u8] = &[
    0x00, 0x00, 0x00, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a,
];
/// Threads each libjxl call may use. Imports already run several files at
/// once, so one encode or decode stays modest.
const JXL_THREADS: &str = "--num_threads=2";

/// Whether `head` starts with a JPEG XL signature.
pub fn is_jxl(head: &[u8]) -> bool {
    head.starts_with(CODESTREAM_SIGNATURE) || head.starts_with(CONTAINER_SIGNATURE)
}

/// Whether the file at `path` is JPEG XL, judged by its first bytes rather
/// than its extension.
pub fn has_signature(path: &Path) -> bool {
    let mut head = Vec::with_capacity(CONTAINER_SIGNATURE.len());
    let read = std::fs::File::open(path).and_then(|file| {
        file.take(CONTAINER_SIGNATURE.len() as u64)
            .read_to_end(&mut head)
    });
    read.is_ok() && is_jxl(&head)
}

/// Decode a JPEG XL file to the pixels the decoder renders. The size limits
/// are checked from the header before any pixel is produced.
pub fn decode(path: &Path) -> Result<DynamicImage> {
    let file = std::fs::File::open(path)
        .map_err(|error| Error::Msg(format!("open image {}: {error}", path.display())))?;
    let mut decoder =
        JxlDecoder::new(BufReader::new(file)).map_err(|error| decode_error(path, error))?;
    let (width, height) = decoder.dimensions();
    decode::check_dimensions(path, width, height)?;
    decoder
        .set_limits(decode::limits())
        .map_err(|error| decode_error(path, error))?;
    DynamicImage::from_decoder(decoder).map_err(|error| decode_error(path, error))
}

fn decode_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Msg(format!("decode image {}: {error}", path.display()))
}

/// What a JPEG XL file holds, read from its header without decoding pixels.
pub struct Header {
    /// Size of the picture the decoder returns, with any rotation applied.
    pub width: u32,
    pub height: u32,
    /// The orientation the codestream carries. 1 means none.
    pub orientation: u32,
}

pub fn header(path: &Path) -> Result<Header> {
    let image = JxlImage::builder()
        .open(path)
        .map_err(|error| Error::Msg(format!("read JPEG XL header {}: {error}", path.display())))?;
    Ok(Header {
        width: image.width(),
        height: image.height(),
        orientation: image.image_header().metadata.orientation,
    })
}

/// Whether the decoder itself turns this file upright. libjxl writes the EXIF
/// rotation of a transcoded JPEG into the codestream, and jxl-oxide applies
/// it while rendering. An orientation taken from the original must then be
/// left alone, or the picture is rotated twice.
pub fn oriented_by_decoder(path: &Path) -> bool {
    has_signature(path) && header(path).is_ok_and(|header| header.orientation != 1)
}

/// Losslessly transcode a JPEG. The JPEG's own bytes travel inside the
/// result, so [`reconstruct_jpeg`] can rebuild the original file exactly.
pub fn transcode_jpeg(tools: &Tools, source: &Path, dest: &Path) -> Result<()> {
    run_encoder(tools, source, dest, &["--lossless_jpeg=1"])
}

/// Losslessly encode a PNG. Every sample is kept, and so are its ICC profile
/// and EXIF block, which libjxl stores as boxes.
pub fn encode_lossless(tools: &Tools, source: &Path, dest: &Path) -> Result<()> {
    run_encoder(tools, source, dest, &["-d", "0", "-e", "7"])
}

fn run_encoder(tools: &Tools, source: &Path, dest: &Path, options: &[&str]) -> Result<()> {
    let mut cmd = ffmpeg::command(tools.cjxl()?);
    cmd.arg(source).arg(dest).args(options).arg(JXL_THREADS);
    let run = ffmpeg::run_bounded(cmd, DEADLINE)?;
    finish("JPEG XL encode", source, dest, &run)
}

/// Rebuild the original JPEG from a file made by [`transcode_jpeg`].
pub fn reconstruct_jpeg(tools: &Tools, source: &Path, dest: &Path) -> Result<()> {
    let mut cmd = ffmpeg::command(tools.djxl()?);
    cmd.arg(source).arg(dest).arg(JXL_THREADS);
    let run = ffmpeg::run_bounded(cmd, DEADLINE)?;
    finish("JPEG XL reconstruction", source, dest, &run)
}

/// A tool's output counts only if the tool succeeded and wrote the file. A
/// failed attempt leaves nothing behind.
fn finish(what: &str, source: &Path, dest: &Path, run: &Bounded) -> Result<()> {
    if run.status.success() && dest.is_file() {
        return Ok(());
    }
    let _ = std::fs::remove_file(dest);
    Err(ffmpeg::failure(what, source, run))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::{GenericImageView, Rgb, RgbImage};

    /// The tools next to the repository's assets, or the directory named by
    /// `FORBIDDEN_LIBRARY_JXL_DIR`. `None` when they are not there.
    fn test_tools() -> Option<Tools> {
        let dir = std::env::var_os("FORBIDDEN_LIBRARY_JXL_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/jxl"));
        let cjxl = dir.join("cjxl.exe");
        let djxl = dir.join("djxl.exe");
        (cjxl.is_file() && djxl.is_file()).then_some(Tools {
            ffmpeg: None,
            ffprobe: None,
            cjxl: Some(cjxl),
            djxl: Some(djxl),
        })
    }

    /// A non-square JPEG whose EXIF orientation is `orientation`. The pixels
    /// are stored as they are; only the tag says how to turn them.
    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x * 7) as u8, (y * 5) as u8, 90])
        });
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&DynamicImage::ImageRgb8(image))
            .unwrap();

        // One IFD holding a single Orientation entry, in a big-endian TIFF.
        let mut tiff = b"MM\0*".to_vec();
        tiff.extend_from_slice(&8u32.to_be_bytes());
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x0112u16.to_be_bytes());
        tiff.extend_from_slice(&3u16.to_be_bytes());
        tiff.extend_from_slice(&1u32.to_be_bytes());
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_be_bytes());

        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(&payload);
        jpeg.splice(2..2, segment);
        jpeg
    }

    #[test]
    fn both_jpeg_xl_signatures_are_recognised() {
        assert!(is_jxl(&[0xff, 0x0a, 0x00]));
        assert!(is_jxl(CONTAINER_SIGNATURE));
        assert!(!is_jxl(&[0xff, 0xd8, 0xff]));
        assert!(!is_jxl(&[]));
    }

    #[test]
    fn a_transcoded_jpeg_is_rebuilt_byte_for_byte() {
        let Some(tools) = test_tools() else {
            eprintln!("skipping: assets/jxl is not present");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let jpg = temp.path().join("source.jpg");
        let original = jpeg_with_orientation(64, 40, 6);
        std::fs::write(&jpg, &original).unwrap();
        let jxl = temp.path().join("source.jxl");
        let rebuilt = temp.path().join("rebuilt.jpg");

        transcode_jpeg(&tools, &jpg, &jxl).unwrap();
        reconstruct_jpeg(&tools, &jxl, &rebuilt).unwrap();
        assert_eq!(std::fs::read(&rebuilt).unwrap(), original);
    }

    #[test]
    fn a_transcoded_jpeg_turns_upright_exactly_once() {
        let Some(tools) = test_tools() else {
            eprintln!("skipping: assets/jxl is not present");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let jpg = temp.path().join("portrait.jpg");
        std::fs::write(&jpg, jpeg_with_orientation(64, 40, 6)).unwrap();
        let jxl = temp.path().join("portrait.jxl");
        transcode_jpeg(&tools, &jpg, &jxl).unwrap();

        // The app's normal path for the JPEG: decode the raw pixels, then turn
        // them by the EXIF orientation.
        let expected = crate::thumb::orient(decode::open(&jpg).unwrap(), 6);
        assert_eq!(expected.dimensions(), (40, 64));

        // The stored JPEG XL, turned by whichever side the rotation belongs to.
        let stored = decode::open(&jxl).unwrap();
        let turn = crate::thumb::orientation_after_decode(&jxl, 6);
        let upright = crate::thumb::orient(stored, turn);
        assert_eq!(upright.dimensions(), expected.dimensions());
    }

    #[test]
    fn a_lossless_encode_decodes_to_the_same_samples_at_sixteen_bits() {
        let Some(tools) = test_tools() else {
            eprintln!("skipping: assets/jxl is not present");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let png = temp.path().join("deep.png");
        let deep = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_fn(96, 64, |x, y| {
            image::Rgb([
                (x * 600 + y) as u16,
                (y * 900 + x) as u16,
                ((x ^ y) * 1000) as u16,
            ])
        });
        DynamicImage::ImageRgb16(deep.clone())
            .save_with_format(&png, image::ImageFormat::Png)
            .unwrap();
        let jxl = temp.path().join("deep.jxl");

        encode_lossless(&tools, &png, &jxl).unwrap();
        let decoded = decode(&jxl).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgb16);
        assert_eq!(decoded.to_rgb16().as_raw(), deep.as_raw());
    }

    #[test]
    fn the_header_reports_the_size_the_decoder_returns() {
        let Some(tools) = test_tools() else {
            eprintln!("skipping: assets/jxl is not present");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let jpg = temp.path().join("portrait.jpg");
        std::fs::write(&jpg, jpeg_with_orientation(64, 40, 6)).unwrap();
        let jxl = temp.path().join("portrait.jxl");
        transcode_jpeg(&tools, &jpg, &jxl).unwrap();

        let head = header(&jxl).unwrap();
        let decoded = decode(&jxl).unwrap();
        assert_eq!((head.width, head.height), decoded.dimensions());
    }
}
