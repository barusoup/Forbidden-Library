//! Format-coverage matrix for the viewer engines. Every file in `FL_MATRIX_DIR`
//! is imported through the real importer, then run through the engine the app
//! would choose for it, and one row is printed per file.
//!
//! Run by hand with:
//! `FL_MATRIX_DIR=<dir> cargo test -p forbidden-library media_matrix -- --ignored --nocapture`
//!
//! Only hard invariants fail the test: no hang past the timeout, a clean
//! `Failed` for garbage input, the display aspect of the rotated and
//! anamorphic clips, and a frame size that never changes mid-stream. Everything
//! else is reported in the table.

use super::anim::{AnimPlayer, AnimStatus};
use super::video::{PlayerStatus, VideoPlayer};
use super::{DecodedFrame, Wake};
use crate::viewer::{ANIM_GPU_BUDGET, ANIM_MAX_SIDE};
use archive_core::{
    FULL_SIDE, ImportOptions, Item, Kind, Library, TimelineFilter, Tools, fit_even, import_paths,
    inspect,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Longest any engine may take to show its first frame or to fail.
const TIMEOUT: Duration = Duration::from_secs(15);
/// How long playback is watched for the frames-shown count.
const PLAY_WINDOW: Duration = Duration::from_millis(1500);
/// The viewer's size on a 1920x1080 screen.
const VIEW: (u32, u32) = (1920, 1080);
/// Videos that also run with audio on. The device may be missing; the row says so.
const AUDIO_CHECK: [&str; 2] = ["h264_aac_1080p30.mp4", "vp9_opus.webm"];
/// Files whose display size is known from the fixture recipe.
const ROTATED: &str = "portrait_rot90.mp4";
const ANAMORPHIC: &str = "anamorphic_sar43.mp4";

const VIDEO_EXTS: [&str; 13] = [
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "ts", "mts", "m2ts", "mpg", "3gp", "flv",
];
const ANIM_EXTS: [&str; 4] = ["gif", "png", "apng", "webp"];

#[derive(Default)]
struct Row {
    file: String,
    import: String,
    engine: String,
    status: String,
    first_ms: Option<u128>,
    frames_1_5s: Option<usize>,
    decoded: Option<(u32, u32)>,
    expected: Option<(u32, u32)>,
    seek: String,
    note: String,
}

fn no_wake() -> Wake {
    Arc::new(|| {})
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn clip(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(limit).collect()
}

fn dims(size: Option<(u32, u32)>) -> String {
    match size {
        Some((w, h)) => format!("{w}x{h}"),
        None => "-".into(),
    }
}

/// Pump a video player until its first frame, a failure, or the timeout.
fn first_video_frame(player: &mut VideoPlayer, started: Instant) -> Option<DecodedFrame> {
    let deadline = started + TIMEOUT;
    loop {
        let now = Instant::now();
        if let Some(frame) = player.poll(now) {
            return Some(frame);
        }
        if matches!(
            player.status(),
            PlayerStatus::Failed(_) | PlayerStatus::Ended
        ) {
            return None;
        }
        if now >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_video(path: &Path, audio: bool, row: &mut Row, violations: &mut Vec<String>) {
    let started = Instant::now();
    let mut player = VideoPlayer::open(path.to_path_buf(), VIEW.0, VIEW.1, no_wake(), audio);
    let first = first_video_frame(&mut player, started);
    let Some(first) = first else {
        let status = player.status();
        row.status = format!("{status:?}");
        row.note = format!("no frame in {} ms", started.elapsed().as_millis());
        if let PlayerStatus::Failed(message) = &status {
            row.note = format!(
                "failed in {} ms: {}",
                started.elapsed().as_millis(),
                clip(message, 70)
            );
        } else if started.elapsed() >= TIMEOUT {
            violations.push(format!(
                "{}: no frame or failure within {TIMEOUT:?}",
                row.file
            ));
        }
        return;
    };
    row.first_ms = Some(started.elapsed().as_millis());
    let first_size = (first.width, first.height);
    row.decoded = Some(first_size);
    player.recycle(first.rgba);

    let info = player.info().cloned();
    let display = info.as_ref().map(|i| (i.width, i.height));
    row.expected = display.map(|(w, h)| fit_even(w, h, VIEW.0, VIEW.1));
    row.status = format!("{:?}", player.status());

    // Playback window: count the frames the viewer would have shown.
    let window_end = Instant::now() + PLAY_WINDOW;
    let mut shown = 0usize;
    let mut resized = false;
    while Instant::now() < window_end {
        if let Some(frame) = player.poll(Instant::now()) {
            shown += 1;
            if (frame.width, frame.height) != first_size {
                resized = true;
            }
            player.recycle(frame.rgba);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    row.frames_1_5s = Some(shown);
    row.status = format!("{:?}", player.status());
    if resized {
        violations.push(format!("{}: frame size changed mid-stream", row.file));
        row.note.push_str(" SIZE-CHANGED");
    }

    // The fixtures with a known display size must come out with that aspect.
    let name = row.file.clone();
    let expected_display = match name.as_str() {
        ROTATED => Some((1080, 1920)),
        ANAMORPHIC => Some((853, 480)),
        _ => None,
    };
    if let Some(want) = expected_display {
        if display != Some(want) {
            violations.push(format!(
                "{name}: display size {display:?}, expected {want:?}"
            ));
        }
        let got = (first_size.0 as f64) / (first_size.1 as f64);
        let aspect = (want.0 as f64) / (want.1 as f64);
        if (got - aspect).abs() / aspect > 0.01 {
            violations.push(format!(
                "{name}: decoded aspect {got:.4}, expected {aspect:.4}"
            ));
        }
    }
    if let Some(expected) = row.expected
        && expected != first_size
    {
        violations.push(format!(
            "{name}: decoded {first_size:?}, player expected {expected:?}"
        ));
    }

    // Seek to the middle while playing and see where the clock lands.
    let duration = player.duration_ms().unwrap_or(0);
    if duration > 1000 {
        let target = duration / 2;
        let seek_at = Instant::now();
        player.seek(target, seek_at);
        let deadline = seek_at + TIMEOUT;
        let mut landed = None;
        while Instant::now() < deadline {
            let now = Instant::now();
            if let Some(frame) = player.poll(now) {
                if now.saturating_duration_since(seek_at) >= Duration::from_millis(150) {
                    landed = Some((now, frame));
                    break;
                }
                player.recycle(frame.rgba);
            }
            if matches!(
                player.status(),
                PlayerStatus::Ended | PlayerStatus::Failed(_)
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        match landed {
            Some((at, frame)) => {
                let position = player.position_ms(at) as i64;
                let error = position - target;
                row.seek = format!(
                    "{}ms -> pos {}ms ({:+}ms) in {}ms",
                    target,
                    position,
                    error,
                    at.duration_since(seek_at).as_millis()
                );
                player.recycle(frame.rgba);
            }
            None => {
                // Ending with no frame is reported, not asserted: it is the
                // player's answer to a seek that decoded nothing. Still
                // loading or playing at the deadline is a hang.
                let status = player.status();
                row.seek = format!("{target}ms: no frame, status {status:?}");
                if matches!(status, PlayerStatus::Loading | PlayerStatus::Playing) {
                    violations.push(format!("{name}: seek to {target} ms hung in {status:?}"));
                }
            }
        }
    } else {
        row.seek = "skipped (no duration)".into();
    }
    row.status = format!("{:?}", player.status());
}

fn run_anim(path: &Path, row: &mut Row, violations: &mut Vec<String>) {
    let side = VIEW.0.max(VIEW.1).min(ANIM_MAX_SIDE);
    let started = Instant::now();
    let mut player = AnimPlayer::open(path.to_path_buf(), side, ANIM_GPU_BUDGET, no_wake());
    let deadline = started + TIMEOUT;
    let mut first = None;
    loop {
        let now = Instant::now();
        if let Some(tick) = player.poll(now) {
            first = Some(tick);
            break;
        }
        if matches!(
            player.status(),
            AnimStatus::NotAnimated | AnimStatus::Failed(_)
        ) {
            break;
        }
        if now >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    row.status = format!("{:?}", player.status());
    let Some(first) = first else {
        if matches!(player.status(), AnimStatus::Loading) {
            violations.push(format!(
                "{}: no frame or status within {TIMEOUT:?}",
                row.file
            ));
        }
        row.note = "no frame".into();
        return;
    };
    row.first_ms = Some(started.elapsed().as_millis());
    if let Some(frame) = &first.frame {
        row.decoded = Some((frame.width, frame.height));
    }
    let window_end = Instant::now() + PLAY_WINDOW;
    let mut shown = 1usize;
    while Instant::now() < window_end {
        if let Some(tick) = player.poll(Instant::now()) {
            shown += 1;
            if let Some(frame) = tick.frame {
                player.recycle(frame.rgba);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    row.frames_1_5s = Some(shown);
    row.status = format!("{:?}", player.status());
}

fn run_still(item: &Item, library: &Library, row: &mut Row) {
    let expected = item.display_size();
    row.expected = expected;
    let started = Instant::now();
    match library.load_full_rgba(item, FULL_SIDE) {
        Ok((w, h, _)) => {
            row.first_ms = Some(started.elapsed().as_millis());
            row.decoded = Some((w, h));
            row.status = "Still".into();
            if let Some(expected) = expected
                && expected != (w, h)
            {
                row.note = format!("MISMATCH: expected {expected:?}");
            }
        }
        Err(error) => {
            row.status = "Failed".into();
            row.note = format!("load: {}", clip(&error.to_string(), 70));
        }
    }
}

#[test]
#[ignore]
fn media_matrix() {
    let Some(dir) = std::env::var_os("FL_MATRIX_DIR").map(PathBuf::from) else {
        eprintln!("skipped: set FL_MATRIX_DIR to the fixture folder");
        return;
    };
    let tools = Tools::discover();
    println!(
        "ffmpeg:  {}",
        tools
            .ffmpeg
            .as_ref()
            .map_or("none".into(), |p| p.display().to_string())
    );
    println!(
        "ffprobe: {}",
        tools
            .ffprobe
            .as_ref()
            .map_or("none".into(), |p| p.display().to_string())
    );

    let scratch = tempfile::TempDir::new().expect("temp dir");
    let library = Library::open(scratch.path().join("library")).expect("library");
    let report = import_paths(
        &library,
        std::slice::from_ref(&dir),
        &ImportOptions::default(),
        &mut |_| {},
    );
    println!(
        "import: {} imported, {} duplicate, {} skipped, {} failed",
        report.imported, report.duplicates, report.skipped, report.failed
    );
    let items = library.items(&TimelineFilter::default()).expect("items");
    let by_name: std::collections::HashMap<String, Item> = items
        .into_iter()
        .map(|item| (item.original_name.clone(), item))
        .collect();
    // The importer keys its failures by full path; the matrix keys by file name.
    let rejected: std::collections::HashMap<String, String> = report
        .failures
        .iter()
        .filter_map(|(path, why)| {
            Path::new(path)
                .file_name()
                .map(|name| (name.to_string_lossy().into_owned(), why.clone()))
        })
        .collect();

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read matrix dir")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file())
        .collect();
    files.sort();

    let mut rows: Vec<Row> = Vec::new();
    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let ext = ext_of(path);
        if name.starts_with('_') {
            continue;
        }
        let import = match by_name.get(&name) {
            Some(item) => format!(
                "{:?}/{} {}",
                item.kind,
                item.ext,
                dims(item.width.zip(item.height))
            ),
            None => match rejected.get(&name) {
                Some(why) => format!("REJECTED: {}", clip(why, 40)),
                None => "not imported".into(),
            },
        };
        let garbage = name.starts_with("bad_");
        let is_video = VIDEO_EXTS.contains(&ext.as_str());
        let is_anim = ANIM_EXTS.contains(&ext.as_str());
        let mut base = Row {
            file: name.clone(),
            import,
            ..Row::default()
        };

        if is_video {
            base.engine = "video".into();
            let mut row = Row {
                file: name.clone(),
                import: base.import.clone(),
                engine: "video".into(),
                ..Row::default()
            };
            run_video(path, false, &mut row, &mut violations);
            if garbage && !row.status.starts_with("Failed") {
                violations.push(format!(
                    "{name}: garbage did not fail cleanly ({})",
                    row.status
                ));
            }
            // The grid thumbnail is the other way a video's display size shows up.
            if let Some(item) = by_name.get(&name) {
                match library.load_thumb_rgba(item) {
                    Ok((w, h, _)) => {
                        row.note = format!("thumb {w}x{h}; {}", row.note)
                            .trim_end_matches("; ")
                            .to_string();
                        if name == ROTATED && w > h {
                            violations.push(format!(
                                "{name}: grid thumbnail is landscape ({w}x{h}) for a portrait clip"
                            ));
                        }
                    }
                    Err(error) => {
                        row.note = format!(
                            "thumb failed: {}; {}",
                            clip(&error.to_string(), 50),
                            row.note
                        )
                    }
                }
            }
            rows.push(row);
            if AUDIO_CHECK.contains(&name.as_str()) {
                let mut row = Row {
                    file: format!("{name} +audio"),
                    import: base.import.clone(),
                    engine: "video".into(),
                    ..Row::default()
                };
                run_video(path, true, &mut row, &mut violations);
                rows.push(row);
            }
        } else if is_anim {
            base.engine = "anim".into();
            run_anim(path, &mut base, &mut violations);
            base.expected = inspect(path, &tools)
                .ok()
                .and_then(|info| info.width.zip(info.height));
            // The app shows a non-animated file as a still, so do the same.
            let not_animated = base.status == "NotAnimated";
            rows.push(base);
            if let (true, Some(item)) = (not_animated, by_name.get(&name)) {
                let mut row = Row {
                    file: format!("{name} (still)"),
                    import: format!("{:?}/{}", item.kind, item.ext),
                    engine: "still".into(),
                    ..Row::default()
                };
                run_still(item, &library, &mut row);
                rows.push(row);
            }
        } else if let Some(item) = by_name.get(&name) {
            if item.kind == Kind::Image {
                base.engine = "still".into();
                run_still(item, &library, &mut base);
            } else {
                base.engine = format!("{:?}", item.kind);
                base.status = "not played".into();
            }
            rows.push(base);
        } else {
            base.engine = "-".into();
            base.status = "no engine for this extension".into();
            rows.push(base);
        }
    }

    println!();
    println!(
        "{:<28} {:<26} {:<6} {:<34} {:>7} {:>7} {:>10} {:>10}  seek / note",
        "file", "import", "engine", "status", "first", "frames", "decoded", "expected"
    );
    for row in &rows {
        let first = row.first_ms.map_or("-".into(), |ms| format!("{ms}ms"));
        let frames = row.frames_1_5s.map_or("-".into(), |n| n.to_string());
        let tail = [row.seek.as_str(), row.note.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        println!(
            "{:<28} {:<26} {:<6} {:<34} {:>7} {:>7} {:>10} {:>10}  {}",
            clip(&row.file, 28),
            clip(&row.import, 26),
            row.engine,
            clip(&row.status, 34),
            first,
            frames,
            dims(row.decoded),
            dims(row.expected),
            tail
        );
    }
    println!();

    assert!(
        violations.is_empty(),
        "hard invariants violated:\n  {}",
        violations.join("\n  ")
    );
}

/// Plays `FL_CPU_CLIP` for six seconds for an outside sampler to measure the
/// CPU time of the process tree during playback. `FL_CPU_AUDIO=1` turns the
/// audio output on. Run through the PowerShell sampler, not by hand.
#[test]
#[ignore]
fn cpu_video_playback_window() {
    let Some(clip) = std::env::var_os("FL_CPU_CLIP").map(PathBuf::from) else {
        eprintln!("skipped: set FL_CPU_CLIP");
        return;
    };
    let audio = std::env::var("FL_CPU_AUDIO").is_ok_and(|v| v == "1");
    let mut player = VideoPlayer::open(clip, VIEW.0, VIEW.1, no_wake(), audio);
    let started = Instant::now();
    let mut frames = 0usize;
    while started.elapsed() < Duration::from_secs(6) {
        let now = Instant::now();
        if let Some(frame) = player.poll(now) {
            frames += 1;
            player.recycle(frame.rgba);
        }
        // Sleep the way the viewer does: until the player's next wake-up,
        // capped so a reader wake-up is never missed for long.
        let wait = player
            .next_wake(now)
            .map_or(Duration::from_millis(5), |at| {
                at.saturating_duration_since(now)
            })
            .min(Duration::from_millis(5));
        std::thread::sleep(wait);
    }
    println!(
        "cpu-window: {frames} frames shown in {:?}",
        started.elapsed()
    );
    assert!(frames > 100, "playback showed only {frames} frames");
}
