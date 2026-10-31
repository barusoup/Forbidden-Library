//! Playback against real ffmpeg output. Each test needs ffmpeg and ffprobe
//! on the PATH and prints a skip note otherwise.
//!
//! The fixtures are made with plain ffmpeg calls rather than the engine's
//! whitelisted helpers, so a bug in the helpers cannot hide inside the test.

use archive_core::{Stream, Tools, VideoOut, playback_info, spawn_audio, spawn_video};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn tools() -> Option<Tools> {
    let tools = Tools::discover();
    if tools.has_ffmpeg() && tools.ffprobe.is_some() {
        Some(tools)
    } else {
        eprintln!("skipped: ffmpeg and ffprobe are not both available");
        None
    }
}

fn run_ffmpeg(ffmpeg: &Path, args: &[&str], dest: &Path) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(dest)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A two-second 320x240, 25 fps clip with a 440 Hz tone. H.264 when this
/// build has libx264, MPEG-4 part 2 otherwise. `extra` are output options.
fn sample_clip(tools: &Tools, dir: &Path, name: &str) -> PathBuf {
    let ffmpeg = tools.ffmpeg.as_deref().unwrap();
    let dest = dir.join(name);
    for codec in ["libx264", "mpeg4"] {
        let args = [
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            codec,
            "-c:a",
            "aac",
        ];
        if run_ffmpeg(ffmpeg, &args, &dest) {
            return dest;
        }
    }
    panic!("ffmpeg could not write the sample clip");
}

/// Read a stream to its end and return how many whole frames of
/// `frame_bytes` it held. A trailing partial frame fails the test.
fn drain_frames(stream: &mut impl Read, frame_bytes: usize) -> usize {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert_eq!(
        bytes.len() % frame_bytes,
        0,
        "stream ended in the middle of a frame"
    );
    bytes.len() / frame_bytes
}

#[test]
fn streams_and_animation_frames_can_cross_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<Stream>();
    assert_send::<Box<dyn Iterator<Item = archive_core::Result<archive_core::AnimFrame>> + Send>>();
}

#[test]
fn playback_info_reports_size_rate_length_and_both_tracks() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let clip = sample_clip(&tools, dir.path(), "clip.mp4");

    let info = playback_info(&tools, &clip).unwrap();
    assert_eq!((info.width, info.height), (320, 240));
    assert!((info.fps - 25.0).abs() < 0.01, "fps was {}", info.fps);
    let duration = info.duration_ms.expect("duration");
    assert!(
        (1900..=2200).contains(&duration),
        "duration was {duration} ms"
    );
    assert!(info.has_video && info.has_audio);
}

#[test]
fn video_from_the_start_is_fifty_frames_then_end_of_stream() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let clip = sample_clip(&tools, dir.path(), "clip.mp4");

    let out = VideoOut {
        width: 160,
        height: 120,
        fps: 25.0,
        hwaccel: false,
        exact_seek: false,
    };
    let mut stream = spawn_video(&tools, &clip, 0, &out).unwrap();
    let mut pipe = stream.stdout().unwrap();
    let frames = drain_frames(&mut pipe, 160 * 120 * 4);
    assert!((48..=52).contains(&frames), "got {frames} frames");
}

#[test]
fn video_from_one_second_is_about_one_second_of_frames() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let clip = sample_clip(&tools, dir.path(), "clip.mp4");

    let out = VideoOut {
        width: 160,
        height: 120,
        fps: 25.0,
        hwaccel: false,
        exact_seek: false,
    };
    let mut stream = spawn_video(&tools, &clip, 1000, &out).unwrap();
    let mut pipe = stream.stdout().unwrap();
    let frames = drain_frames(&mut pipe, 160 * 120 * 4);
    assert!((23..=27).contains(&frames), "got {frames} frames");
}

#[test]
fn audio_is_two_channels_of_float_pcm_at_the_requested_rate() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let clip = sample_clip(&tools, dir.path(), "clip.mp4");

    let mut stream = spawn_audio(&tools, &clip, 0, 48_000, 2).unwrap();
    let mut pipe = stream.stdout().unwrap();
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len() % 4, 0, "samples are 32-bit floats");
    let samples = bytes.len() / 4;
    let expected = 2 * 48_000 * 2;
    let low = expected * 90 / 100;
    let high = expected * 110 / 100;
    assert!(
        (low..=high).contains(&samples),
        "got {samples} samples, expected about {expected}"
    );
}

#[test]
fn dropping_a_stream_mid_read_ends_the_tool_quickly() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let clip = sample_clip(&tools, dir.path(), "clip.mp4");

    let out = VideoOut {
        width: 160,
        height: 120,
        fps: 25.0,
        hwaccel: false,
        exact_seek: false,
    };
    let started = Instant::now();
    let mut stream = spawn_video(&tools, &clip, 0, &out).unwrap();
    let mut pipe = stream.stdout().unwrap();
    let mut first = vec![0u8; 160 * 120 * 4];
    pipe.read_exact(&mut first).unwrap();
    drop(stream);
    // Killing the tool closes its end of the pipe, so the rest reads to EOF.
    let mut rest = Vec::new();
    pipe.read_to_end(&mut rest).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "dropping the stream did not end the tool"
    );
}

#[test]
fn a_failing_tool_explains_itself_on_stderr() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("missing.mp4");
    let out = VideoOut {
        width: 160,
        height: 120,
        fps: 25.0,
        hwaccel: false,
        exact_seek: false,
    };
    let mut stream = spawn_video(&tools, &missing, 0, &out).unwrap();
    let mut pipe = stream.stdout().unwrap();
    let mut rest = Vec::new();
    pipe.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());

    // The stderr pipe is drained on its own thread, which may finish a
    // moment after stdout reaches EOF.
    let started = Instant::now();
    while stream.stderr_text().is_empty() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let reason = stream.stderr_text();
    assert!(!reason.is_empty(), "no diagnostic was captured");
    assert!(
        reason.contains("missing.mp4") || reason.to_lowercase().contains("no such file"),
        "unexpected diagnostic: {reason}"
    );
}

#[test]
fn a_rotated_clip_reports_swapped_dimensions() {
    let Some(tools) = tools() else { return };
    let dir = TempDir::new().unwrap();
    let base = sample_clip(&tools, dir.path(), "base.mp4");
    let ffmpeg = tools.ffmpeg.as_deref().unwrap();
    // A stream copy with a quarter turn on the input writes a display matrix
    // and leaves the pixels alone. Builds that refuse the option are skipped:
    // `-metadata:s:v:0 rotate=90` is not written as a display matrix by every
    // muxer, so it cannot be relied on to produce a rotated fixture.
    let rotated = dir.path().join("rotated.mp4");
    let base_arg = base.to_str().unwrap();
    if run_ffmpeg(
        ffmpeg,
        &["-display_rotation", "90", "-i", base_arg, "-c", "copy"],
        &rotated,
    ) {
        let info = playback_info(&tools, &rotated).unwrap();
        assert_eq!((info.width, info.height), (240, 320));
    } else {
        eprintln!("note: this ffmpeg build cannot write -display_rotation; rotation check skipped");
    }
}

/// A real phone HEIC, given by path in `FL_HEIC_SAMPLE`. No encoder in the
/// bundled FFmpeg can write a tiled HEIF, so there is no generated fixture;
/// run with `FL_HEIC_SAMPLE=<file> cargo test -p archive-core heic -- --ignored`.
#[test]
#[ignore]
fn a_heic_sample_imports_previews_and_decodes_in_full() {
    let Some(sample) = std::env::var_os("FL_HEIC_SAMPLE").map(PathBuf::from) else {
        eprintln!("skipped: FL_HEIC_SAMPLE is not set");
        return;
    };
    let temp = TempDir::new().unwrap();
    let library = archive_core::Library::open(temp.path().join("library")).unwrap();
    let report = archive_core::import_paths(
        &library,
        &[sample],
        &archive_core::ImportOptions::default(),
        &mut |_| {},
    );
    assert_eq!(report.imported, 1, "{report:?}");
    let ids = library
        .item_ids(&archive_core::TimelineFilter::default())
        .unwrap();
    let item = library.item(&ids[0]).unwrap().unwrap();
    assert_eq!(item.kind, archive_core::Kind::Image);
    let (width, height) = (item.width.unwrap(), item.height.unwrap());
    eprintln!("probed {width}x{height}, orientation {}", item.orientation);
    assert!(width.max(height) > 512, "probed one tile, not the image");

    library.make_thumbnail(&item).unwrap();
    let (pw, ph, _) = library
        .load_preview_rgba(&item, archive_core::VIEWER_SIDE)
        .unwrap();
    let (fw, fh, _) = library
        .load_full_rgba(&item, archive_core::FULL_SIDE)
        .unwrap();
    eprintln!("preview {pw}x{ph}, full {fw}x{fh}");
    assert_eq!(pw.max(ph), archive_core::VIEWER_SIDE.min(width.max(height)));
    assert_eq!((fw, fh), (width, height));
}
