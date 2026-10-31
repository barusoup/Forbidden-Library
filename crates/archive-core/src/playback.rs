//! What a player needs to know before it starts decoding: the display size,
//! frame rate and length of a file, read from ffprobe's JSON.
//!
//! The size already has rotation and pixel aspect applied, so the player can
//! size its frame buffer and texture from these numbers alone, and ask ffmpeg
//! to scale to exactly that size.

use crate::error::{Error, Result};
use crate::ffmpeg::{self, Tools};
use serde_json::Value;
use std::path::Path;

/// Used when a file does not state a usable frame rate.
const DEFAULT_FPS: f64 = 30.0;

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackInfo {
    /// Display size: rotation and sample aspect ratio applied.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_ms: Option<i64>,
    pub has_video: bool,
    pub has_audio: bool,
}

/// Probe `source` with ffprobe. Fails when the file has no playable stream.
pub fn playback_info(tools: &Tools, source: &Path) -> Result<PlaybackInfo> {
    tools.ffprobe()?;
    let json = ffmpeg::ffprobe_json(tools, source)
        .ok_or_else(|| Error::Msg(format!("ffprobe could not read {}", source.display())))?;
    playback_info_from_json(&json).ok_or_else(|| {
        Error::Msg(format!(
            "{} has no playable audio or video stream",
            source.display()
        ))
    })
}

/// Read ffprobe's `-show_format -show_streams` JSON. Pure, so it can be tested
/// with hand-written documents.
pub fn playback_info_from_json(json: &Value) -> Option<PlaybackInfo> {
    let streams = json.get("streams")?.as_array()?;
    let has_audio = streams.iter().any(|stream| is_type(stream, "audio"));
    // Cover art is an attached picture, not a video track.
    // A video stream with no size cannot be drawn, so it is treated as absent.
    let video = streams
        .iter()
        .find(|stream| is_type(stream, "video") && !is_attached_picture(stream))
        .filter(|stream| stream_size(stream).is_some());

    let duration_ms = duration_ms(json, video);
    let Some(video) = video else {
        if !has_audio {
            return None;
        }
        return Some(PlaybackInfo {
            width: 0,
            height: 0,
            fps: DEFAULT_FPS,
            duration_ms,
            has_video: false,
            has_audio,
        });
    };

    let (mut width, mut height) = stream_size(video)?;
    if let Some((num, den)) = sample_aspect(video) {
        width = (f64::from(width) * num / den).round() as u32;
    }
    // A quarter turn (90 or 270 degrees, either direction) stands the frame up.
    if rotation_degrees(video).is_some_and(|degrees| (degrees.round() as i64).abs() % 180 == 90) {
        std::mem::swap(&mut width, &mut height);
    }

    Some(PlaybackInfo {
        width,
        height,
        fps: frame_rate(video),
        duration_ms,
        has_video: true,
        has_audio,
    })
}

/// Fit `width` x `height` inside `max_w` x `max_h`, keeping the aspect ratio.
/// Never enlarges, and rounds each side down to an even number (at least 2),
/// which is what the 4:2:0 scaler needs.
pub fn fit_even(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_w == 0 || max_h == 0 {
        return (2, 2);
    }
    // Integer arithmetic keeps the fit exact; floats could land one pixel off.
    let (mut w, mut h) = (u64::from(width), u64::from(height));
    let (box_w, box_h) = (u64::from(max_w), u64::from(max_h));
    if w > box_w || h > box_h {
        if w * box_h > h * box_w {
            // Wider than the box: the width is the limit.
            h = h * box_w / w;
            w = box_w;
        } else {
            w = w * box_h / h;
            h = box_h;
        }
    }
    (even_floor(w), even_floor(h))
}

fn even_floor(value: u64) -> u32 {
    (value & !1).max(2) as u32
}

fn is_type(stream: &Value, kind: &str) -> bool {
    stream.get("codec_type").and_then(Value::as_str) == Some(kind)
}

fn is_attached_picture(stream: &Value) -> bool {
    stream
        .get("disposition")
        .and_then(|disposition| disposition.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
}

/// Coded size of a video stream, or `None` when either side is missing or zero.
fn stream_size(stream: &Value) -> Option<(u32, u32)> {
    let width = u32::try_from(stream.get("width")?.as_u64()?).ok()?;
    let height = u32::try_from(stream.get("height")?.as_u64()?).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// The pixel aspect ratio as `(a, b)` when it is set and not square.
fn sample_aspect(stream: &Value) -> Option<(f64, f64)> {
    let text = stream.get("sample_aspect_ratio")?.as_str()?;
    let (num, den) = text.split_once(':')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    (num > 0.0 && den > 0.0 && num != den).then_some((num, den))
}

/// Rotation in degrees. Modern files carry it as display-matrix side data,
/// older ones as a `rotate` tag; the side data wins when both are present.
fn rotation_degrees(stream: &Value) -> Option<f64> {
    let from_side_data = stream
        .get("side_data_list")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find_map(|entry| number(entry.get("rotation")?)));
    from_side_data.or_else(|| {
        stream
            .get("tags")
            .and_then(|tags| tags.get("rotate"))
            .and_then(number)
    })
}

fn frame_rate(stream: &Value) -> f64 {
    ["avg_frame_rate", "r_frame_rate"]
        .iter()
        .find_map(|key| stream.get(*key)?.as_str().and_then(parse_rate))
        .map(|fps| fps.clamp(1.0, 120.0))
        .unwrap_or(DEFAULT_FPS)
}

/// Parse an ffprobe rational such as `30000/1001`. A zero denominator (the
/// `0/0` ffprobe prints for unknown rates) is rejected, not divided by.
fn parse_rate(text: &str) -> Option<f64> {
    let (num, den) = text.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 {
        return None;
    }
    let value = num / den;
    (value.is_finite() && value > 0.0).then_some(value)
}

/// The container duration, or the video stream's own when the container
/// does not say. Either must be a finite, positive number of seconds.
fn duration_ms(json: &Value, video: Option<&Value>) -> Option<i64> {
    let container = json.get("format").and_then(|format| format.get("duration"));
    let stream = video.and_then(|video| video.get("duration"));
    [container, stream]
        .into_iter()
        .flatten()
        .filter_map(number)
        .find(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| (seconds * 1000.0).round() as i64)
}

/// ffprobe writes most numbers as strings, but side data uses real numbers.
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn probe(streams: Value, format: Value) -> Option<PlaybackInfo> {
        playback_info_from_json(&json!({ "streams": streams, "format": format }))
    }

    #[test]
    fn a_plain_widescreen_video_with_audio() {
        let info = probe(
            json!([
                {
                    "codec_type": "video",
                    "width": 1920,
                    "height": 1080,
                    "avg_frame_rate": "30000/1001",
                    "r_frame_rate": "30000/1001",
                    "duration": "10.0"
                },
                { "codec_type": "audio" }
            ]),
            json!({ "duration": "10.010000" }),
        )
        .unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert!((info.fps - 29.97).abs() < 0.01);
        assert_eq!(info.duration_ms, Some(10_010));
        assert!(info.has_video && info.has_audio);
    }

    #[test]
    fn a_negative_side_data_rotation_swaps_the_size() {
        let info = probe(
            json!([{
                "codec_type": "video",
                "width": 1920,
                "height": 1080,
                "avg_frame_rate": "30/1",
                "side_data_list": [
                    { "side_data_type": "Display Matrix", "rotation": -90 }
                ]
            }]),
            json!({ "duration": "2.0" }),
        )
        .unwrap();
        assert_eq!((info.width, info.height), (1080, 1920));
    }

    #[test]
    fn a_rotate_tag_is_read_when_there_is_no_side_data() {
        let info = probe(
            json!([{
                "codec_type": "video",
                "width": 1280,
                "height": 720,
                "avg_frame_rate": "30/1",
                "tags": { "rotate": "90" }
            }]),
            json!({ "duration": "2.0" }),
        )
        .unwrap();
        assert_eq!((info.width, info.height), (720, 1280));
    }

    #[test]
    fn a_half_turn_keeps_the_size() {
        let info = probe(
            json!([{
                "codec_type": "video",
                "width": 1280,
                "height": 720,
                "avg_frame_rate": "30/1",
                "tags": { "rotate": "180" }
            }]),
            json!({ "duration": "2.0" }),
        )
        .unwrap();
        assert_eq!((info.width, info.height), (1280, 720));
    }

    #[test]
    fn a_non_square_pixel_aspect_stretches_the_width() {
        let info = probe(
            json!([{
                "codec_type": "video",
                "width": 720,
                "height": 480,
                "sample_aspect_ratio": "4:3",
                "avg_frame_rate": "30/1"
            }]),
            json!({ "duration": "2.0" }),
        )
        .unwrap();
        assert_eq!((info.width, info.height), (960, 480));
    }

    #[test]
    fn cover_art_and_audio_only_is_not_video() {
        let info = probe(
            json!([
                {
                    "codec_type": "video",
                    "width": 500,
                    "height": 500,
                    "disposition": { "attached_pic": 1 }
                },
                { "codec_type": "audio" }
            ]),
            json!({ "duration": "215.5" }),
        )
        .unwrap();
        assert!(!info.has_video);
        assert!(info.has_audio);
        assert_eq!((info.width, info.height), (0, 0));
        assert_eq!(info.fps, 30.0);
        assert_eq!(info.duration_ms, Some(215_500));
    }

    #[test]
    fn a_zero_average_rate_falls_back_to_the_real_rate() {
        let info = probe(
            json!([{
                "codec_type": "video",
                "width": 640,
                "height": 360,
                "avg_frame_rate": "0/0",
                "r_frame_rate": "25/1"
            }]),
            json!({ "duration": "1.0" }),
        )
        .unwrap();
        assert_eq!(info.fps, 25.0);
    }

    #[test]
    fn a_missing_rate_defaults_to_thirty_and_is_clamped() {
        let missing = probe(
            json!([{ "codec_type": "video", "width": 64, "height": 64 }]),
            json!({}),
        )
        .unwrap();
        assert_eq!(missing.fps, 30.0);
        assert_eq!(missing.duration_ms, None);

        let fast = probe(
            json!([{
                "codec_type": "video",
                "width": 64,
                "height": 64,
                "avg_frame_rate": "1000/1"
            }]),
            json!({}),
        )
        .unwrap();
        assert_eq!(fast.fps, 120.0);
    }

    #[test]
    fn nothing_playable_is_none() {
        assert_eq!(probe(json!([]), json!({})), None);
        assert_eq!(
            probe(
                json!([{ "codec_type": "subtitle" }]),
                json!({ "duration": "3" })
            ),
            None
        );
        assert_eq!(playback_info_from_json(&json!({})), None);
    }

    #[test]
    fn fit_even_keeps_aspect_and_rounds_to_even() {
        assert_eq!(fit_even(1921, 1081, 1920, 1080), (1918, 1080));
        assert_eq!(fit_even(1000, 563, 500, 500), (500, 280));
    }

    #[test]
    fn fit_even_never_upscales() {
        assert_eq!(fit_even(640, 480, 1920, 1080), (640, 480));
        assert_eq!(fit_even(641, 481, 1920, 1080), (640, 480));
    }

    #[test]
    fn fit_even_handles_tiny_and_zero_sizes() {
        assert_eq!(fit_even(1, 1, 1920, 1080), (2, 2));
        assert_eq!(fit_even(1, 10_000, 100, 100), (2, 100));
        assert_eq!(fit_even(0, 0, 1920, 1080), (2, 2));
        assert_eq!(fit_even(1920, 1080, 0, 0), (2, 2));
    }
}
