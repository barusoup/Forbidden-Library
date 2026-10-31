//! Animated GIF, APNG and WebP.
//!
//! Formats are told apart by their first bytes, never by the extension: the
//! extension is whatever the importer was handed, and the decoders below are
//! only ever run on a file whose signature has already been checked.

use crate::decode;
use crate::error::{Error, Result};
use crate::thumb::fit_dims;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::{self, FilterType};
use image::{AnimationDecoder, Delay, Frame, ImageDecoder};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// How much of a file the chunk walkers read. Animation markers come before
/// the first image data, so the header is always enough.
const HEAD_LIMIT: u64 = 64 * 1024;
/// Frames decoded ahead of the reader. Two keeps playback smooth while
/// bounding memory: a 2048 px frame is about 16 MB of RGBA.
const FRAME_QUEUE: usize = 2;

/// One composited frame of an animation, as RGBA8 on the full canvas.
pub struct AnimFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Format {
    Gif,
    Png,
    WebP,
}

/// Whether the file is an animation. Any read or decode error counts as
/// "not animated": the still-image path can still show the file.
pub fn is_animated(path: &Path) -> bool {
    let result = match sniff(path) {
        Some(Format::Gif) => gif_has_two_frames(path),
        Some(Format::Png) => png_has_actl(path),
        Some(Format::WebP) => webp_is_animated(path),
        None => Ok(false),
    };
    result.unwrap_or(false)
}

/// Decode an animation frame by frame. Decoding runs on a helper thread that
/// stays a couple of frames ahead, so the iterator can be handed to a worker.
/// Dropping the iterator stops the helper.
pub fn frames(
    path: &Path,
    max_side: u32,
) -> Result<Box<dyn Iterator<Item = Result<AnimFrame>> + Send>> {
    let format = sniff(path).ok_or_else(|| {
        Error::Msg(format!(
            "{} is not an animated GIF, PNG or WebP",
            path.display()
        ))
    })?;
    let reader = BufReader::new(File::open(path)?);
    Ok(match format {
        Format::Gif => {
            let mut decoder = GifDecoder::new(reader)?;
            decoder.set_limits(decode::limits())?;
            spawn_frames(decoder, max_side)
        }
        Format::Png => {
            // Limits are set on the PNG decoder: the APNG wrapper has no
            // limit setter of its own.
            let mut decoder = PngDecoder::new(reader)?;
            decoder.set_limits(decode::limits())?;
            spawn_frames(decoder.apng()?, max_side)
        }
        Format::WebP => {
            let mut decoder = WebPDecoder::new(reader)?;
            decoder.set_limits(decode::limits())?;
            spawn_frames(decoder, max_side)
        }
    })
}

/// Run `decoder` on a helper thread and hand its frames back over a bounded
/// channel. image's frame iterator is not `Send`, so it is created and used
/// entirely on that thread; only the decoder (which is) moves across.
fn spawn_frames<D>(decoder: D, max_side: u32) -> Box<dyn Iterator<Item = Result<AnimFrame>> + Send>
where
    D: AnimationDecoder<'static> + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(FRAME_QUEUE);
    std::thread::spawn(move || {
        for frame in decoder.into_frames() {
            let decoded = frame
                .map(|frame| to_anim_frame(frame, max_side))
                .map_err(Error::from);
            // A decode error ends the animation; the reader has seen it.
            let stop = decoded.is_err();
            if sender.send(decoded).is_err() || stop {
                break;
            }
        }
    });
    Box::new(receiver.into_iter())
}

/// Turn a composited frame into RGBA and a browser-style delay, scaled to
/// `max_side` when the canvas is larger than that.
fn to_anim_frame(frame: Frame, max_side: u32) -> AnimFrame {
    let delay_ms = browser_delay(frame.delay());
    let mut canvas = frame.into_buffer();
    let (width, height) = canvas.dimensions();
    if max_side > 0 && width.max(height) > max_side {
        let (target_w, target_h) = fit_dims(width, height, max_side);
        // Triangle is plenty for playback and far cheaper than Lanczos3 on
        // every frame of a long animation.
        canvas = imageops::resize(&canvas, target_w, target_h, FilterType::Triangle);
    }
    let (width, height) = canvas.dimensions();
    AnimFrame {
        width,
        height,
        rgba: canvas.into_raw(),
        delay_ms,
    }
}

/// Browsers show a frame for 100 ms when its delay is under 20 ms, since
/// many tools write "as fast as possible" as zero. Matching them keeps
/// animations at the speed their authors saw.
fn browser_delay(delay: Delay) -> u32 {
    let (numer, denom) = delay.numer_denom_ms();
    let ms = u64::from(numer) / u64::from(denom.max(1));
    if ms < 20 {
        100
    } else {
        u32::try_from(ms).unwrap_or(u32::MAX)
    }
}

/// Identify the format from the first bytes of the file.
fn sniff(path: &Path) -> Option<Format> {
    let head = read_head(path, 12).ok()?;
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(Format::Png)
    } else if head.len() == 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some(Format::WebP)
    } else {
        None
    }
}

fn read_head(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut head)?;
    Ok(head)
}

/// GIF has no header flag for animation, so it is decided by decoding. Two
/// frames is enough to call it an animation, and stops the decoder early.
fn gif_has_two_frames(path: &Path) -> Result<bool> {
    let mut decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
    decoder.set_limits(decode::limits())?;
    let frames = decoder
        .into_frames()
        .take(2)
        .collect::<image::ImageResult<Vec<_>>>()?;
    Ok(frames.len() == 2)
}

/// A PNG is an APNG when its `acTL` chunk comes before the first `IDAT`.
/// Chunks are a big-endian length, a four-letter type, the data and a CRC.
fn png_has_actl(path: &Path) -> Result<bool> {
    let head = read_head(path, HEAD_LIMIT)?;
    let mut at = 8;
    while at + 8 <= head.len() {
        let len = u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let kind = &head[at + 4..at + 8];
        if kind == b"acTL" {
            return Ok(true);
        }
        if kind == b"IDAT" {
            return Ok(false);
        }
        at = at
            .saturating_add(12)
            .saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
    }
    Ok(false)
}

/// A WebP is animated when its VP8X header sets the animation flag, or when
/// it carries an `ANIM` chunk. Chunks are a four-letter type, a little-endian
/// length, and data padded to an even length.
fn webp_is_animated(path: &Path) -> Result<bool> {
    let head = read_head(path, HEAD_LIMIT)?;
    let mut at = 12;
    while at + 8 <= head.len() {
        let kind = &head[at..at + 4];
        let len = u32::from_le_bytes([head[at + 4], head[at + 5], head[at + 6], head[at + 7]]);
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let data = at + 8;
        match kind {
            b"ANIM" => return Ok(true),
            b"VP8X" => {
                // The animation bit is 0x02 of the first flags byte.
                if head.get(data).is_some_and(|flags| flags & 0x02 != 0) {
                    return Ok(true);
                }
            }
            // The first image data ends the header.
            b"VP8 " | b"VP8L" => return Ok(false),
            _ => {}
        }
        at = data.saturating_add(len).saturating_add(len & 1);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::{Tools, command};
    use image::codecs::gif::GifEncoder;
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};
    use tempfile::TempDir;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba(color))
    }

    fn write_gif(path: &Path, frames: &[(RgbaImage, u32)]) {
        let file = File::create(path).unwrap();
        let mut encoder = GifEncoder::new(file);
        encoder
            .encode_frames(frames.iter().map(|(image, delay_ms)| {
                Frame::from_parts(
                    image.clone(),
                    0,
                    0,
                    Delay::from_numer_denom_ms(*delay_ms, 1),
                )
            }))
            .unwrap();
    }

    fn three_frame_gif(path: &Path) {
        write_gif(
            path,
            &[
                (solid(48, 32, [255, 0, 0, 255]), 50),
                (solid(48, 32, [0, 255, 0, 255]), 0),
                (solid(48, 32, [0, 0, 255, 255]), 120),
            ],
        );
    }

    #[test]
    fn a_three_frame_gif_is_animated_and_yields_each_frame() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spin.gif");
        three_frame_gif(&path);

        assert!(is_animated(&path));
        let decoded: Vec<AnimFrame> = frames(&path, 0)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(decoded.len(), 3);
        for frame in &decoded {
            assert_eq!((frame.width, frame.height), (48, 32));
            assert_eq!(frame.rgba.len(), 48 * 32 * 4);
        }
        // Delays: 50 ms as written, 0 ms raised to the browser's 100 ms,
        // and 120 ms as written.
        let delays: Vec<u32> = decoded.iter().map(|frame| frame.delay_ms).collect();
        assert_eq!(delays, vec![50, 100, 120]);
        // The first frame is red, the second green (dominant channel check,
        // since GIF palettes are not exact).
        assert!(decoded[0].rgba[0] > 200 && decoded[0].rgba[1] < 60);
        assert!(decoded[1].rgba[1] > 200 && decoded[1].rgba[0] < 60);
    }

    #[test]
    fn frames_are_downscaled_to_the_requested_side() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wide.gif");
        write_gif(
            &path,
            &[
                (solid(200, 100, [1, 2, 3, 255]), 40),
                (solid(200, 100, [4, 5, 6, 255]), 40),
            ],
        );
        let sizes: Vec<(u32, u32)> = frames(&path, 64)
            .unwrap()
            .map(|frame| {
                let frame = frame.unwrap();
                (frame.width, frame.height)
            })
            .collect();
        assert_eq!(sizes, vec![(64, 32), (64, 32)]);
    }

    #[test]
    fn a_single_frame_gif_is_not_animated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("still.gif");
        write_gif(&path, &[(solid(16, 16, [9, 9, 9, 255]), 100)]);
        assert!(!is_animated(&path));
        assert_eq!(frames(&path, 0).unwrap().count(), 1);
    }

    #[test]
    fn a_plain_png_and_a_jpeg_are_not_animated() {
        let dir = TempDir::new().unwrap();
        let png = dir.path().join("photo.png");
        let pixels = RgbImage::from_pixel(16, 16, Rgb([1, 2, 3]));
        let file = File::create(&png).unwrap();
        PngEncoder::new(file)
            .write_image(pixels.as_raw(), 16, 16, ExtendedColorType::Rgb8)
            .unwrap();
        assert!(!is_animated(&png));

        let jpeg = dir.path().join("photo.jpg");
        let file = File::create(&jpeg).unwrap();
        JpegEncoder::new(file)
            .encode(pixels.as_raw(), 16, 16, ExtendedColorType::Rgb8)
            .unwrap();
        assert!(!is_animated(&jpeg));
        // Not an animated format at all, so there is nothing to iterate.
        assert!(frames(&jpeg, 0).is_err());
    }

    #[test]
    fn the_extension_does_not_decide_the_format() {
        let dir = TempDir::new().unwrap();
        let disguised = dir.path().join("spin.png");
        three_frame_gif(&disguised);
        assert!(is_animated(&disguised));
        assert_eq!(frames(&disguised, 0).unwrap().count(), 3);
    }

    #[test]
    fn an_apng_written_by_ffmpeg_is_animated() {
        let tools = Tools::discover();
        let Some(ffmpeg) = tools.ffmpeg.as_deref() else {
            eprintln!("skipped: ffmpeg is not available");
            return;
        };
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("loop.apng");
        let status = command(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10",
                "-t",
                "0.3",
                "-f",
                "apng",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(is_animated(&path));
        assert_eq!(frames(&path, 0).unwrap().count(), 3);
    }

    #[test]
    fn an_animated_webp_written_by_ffmpeg_is_animated() {
        let tools = Tools::discover();
        let Some(ffmpeg) = tools.ffmpeg.as_deref() else {
            eprintln!("skipped: ffmpeg is not available");
            return;
        };
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("loop.webp");
        // The VP8X header carries the animation flag, which is what the
        // header walk looks for before any image data.
        let status = command(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10",
                "-t",
                "0.5",
                "-c:v",
                "libwebp",
                "-loop",
                "0",
                "-f",
                "webp",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        if !status.success() {
            eprintln!("skipped: this ffmpeg build cannot encode animated WebP");
            return;
        }
        assert!(is_animated(&path));
        let decoded: Vec<AnimFrame> = frames(&path, 0)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(decoded.len(), 5);
        assert_eq!((decoded[0].width, decoded[0].height), (64, 48));
    }
}
