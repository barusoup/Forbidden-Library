//! Music through the whole import path: detection, tags, cover art, and the
//! re-encode policy. Every fixture is generated with ffmpeg, and the tests are
//! skipped when no ffmpeg is installed.

use archive_core::{
    ImportOptions, Kind, Library, OptState, OptimizeConfig, ThumbState, Tools, import_paths,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn ffmpeg() -> Option<PathBuf> {
    Tools::discover().ffmpeg
}

/// Run ffmpeg with `args`, panicking with its stderr if it fails.
fn run_ffmpeg(ffmpeg: &Path, args: &[&str]) {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg starts");
    assert!(
        output.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A two-second stereo tone, as 16-bit PCM WAV.
fn tone_wav(ffmpeg: &Path, dir: &Path) -> PathBuf {
    let path = dir.join("tone.wav");
    run_ffmpeg(
        ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-ac",
            "2",
            "-ar",
            "44100",
            "-c:a",
            "pcm_s16le",
            path.to_str().unwrap(),
        ],
    );
    path
}

fn cover_jpeg(ffmpeg: &Path, dir: &Path) -> PathBuf {
    let path = dir.join("cover.jpg");
    run_ffmpeg(
        ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=600x600",
            "-frames:v",
            "1",
            path.to_str().unwrap(),
        ],
    );
    path
}

/// A tagged MP3 at `bitrate`, with cover art when `cover` is given.
fn tagged_mp3(ffmpeg: &Path, wav: &Path, cover: Option<&Path>, bitrate: &str, out: &Path) {
    let mut args = vec!["-i", wav.to_str().unwrap()];
    if let Some(cover) = cover {
        args.extend(["-i", cover.to_str().unwrap(), "-map", "0:a", "-map", "1:v"]);
        args.extend(["-c:v", "mjpeg", "-disposition:v", "attached_pic"]);
    }
    args.extend([
        "-c:a",
        "libmp3lame",
        "-b:a",
        bitrate,
        "-metadata",
        "title=Song",
        "-metadata",
        "artist=Band",
        "-metadata",
        "album=Record",
        "-metadata",
        "album_artist=Band",
        "-metadata",
        "track=3/12",
        "-metadata",
        "disc=1",
        "-metadata",
        "date=2019",
    ]);
    args.push(out.to_str().unwrap());
    run_ffmpeg(ffmpeg, &args);
}

fn optimizing() -> ImportOptions {
    ImportOptions {
        optimize: Some(OptimizeConfig::default()),
        ..ImportOptions::default()
    }
}

/// Import `paths` and return the ids of what was created.
fn import(
    library: &Library,
    paths: &[PathBuf],
    options: &ImportOptions,
) -> archive_core::ImportReport {
    let mut events = Vec::new();
    let report = import_paths(library, paths, options, &mut |event| events.push(event));
    assert!(
        report.failures.is_empty(),
        "failures: {:?}",
        report.failures
    );
    report
}

#[test]
fn lossless_audio_is_stored_as_128k_opus_and_the_original_is_still_recognised() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("library");
    let library = Library::open(&root).unwrap();
    let wav = tone_wav(&ffmpeg, dir.path());
    let original_bytes = std::fs::metadata(&wav).unwrap().len();

    let report = import(&library, std::slice::from_ref(&wav), &optimizing());
    assert_eq!(report.imported, 1);
    let id = report.created[0].clone();
    let item = library.item(&id).unwrap().expect("the item is catalogued");
    assert_eq!(item.kind, Kind::Audio);
    assert_eq!(item.ext, "opus");
    assert_eq!(item.original_ext, "wav");
    assert_eq!(item.opt_state, OptState::Optimized);
    assert_eq!(item.opt_kind.as_deref(), Some("opus"));
    assert_eq!(item.original_bytes, original_bytes);
    assert!(
        item.bytes * 5 < original_bytes,
        "128 kbps Opus should be far smaller than 16-bit PCM ({} against {original_bytes})",
        item.bytes
    );
    assert_eq!(
        item.thumb_state,
        ThumbState::None,
        "no cover, so no preview"
    );
    let meta = library.audio_meta(&item.id).unwrap().unwrap();
    assert_eq!(meta.codec, "opus");

    // Importing the original again is a duplicate of the first copy, not a
    // second archived file, even though the stored bytes are now Opus.
    let again = import(&library, std::slice::from_ref(&wav), &optimizing());
    assert_eq!(again.imported, 0);
    assert_eq!(again.duplicates, 1);
}

#[test]
fn high_bitrate_mp3_becomes_opus_and_keeps_its_tags_and_cover() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let library = Library::open(dir.path().join("library")).unwrap();
    let wav = tone_wav(&ffmpeg, dir.path());
    let cover = cover_jpeg(&ffmpeg, dir.path());
    let mp3 = dir.path().join("song320.mp3");
    tagged_mp3(&ffmpeg, &wav, Some(&cover), "320k", &mp3);

    let report = import(&library, std::slice::from_ref(&mp3), &optimizing());
    assert_eq!(report.imported, 1, "failures: {:?}", report.failures);
    let item = library.item(&report.created[0]).unwrap().unwrap();
    assert_eq!(item.ext, "opus");
    assert_eq!(item.original_ext, "mp3");
    assert_eq!(item.opt_state, OptState::Optimized);
    assert_eq!(item.opt_kind.as_deref(), Some("opus"));
    assert!(item.bytes < item.original_bytes);
    assert_eq!(
        item.thumb_state,
        ThumbState::Ready,
        "the cover is the preview"
    );

    let meta = library
        .audio_meta(&item.id)
        .unwrap()
        .expect("music details are stored");
    assert_eq!(meta.codec, "opus");
    assert!(meta.has_cover);
    assert_eq!(meta.tags.title.as_deref(), Some("Song"));
    assert_eq!(meta.tags.artist.as_deref(), Some("Band"));
    assert_eq!(meta.tags.album.as_deref(), Some("Record"));
    assert_eq!(meta.tags.album_artist.as_deref(), Some("Band"));
    assert_eq!(meta.tags.track_no, Some(3));
    assert_eq!(meta.tags.track_total, Some(12));
    assert_eq!(meta.tags.disc_no, Some(1));
    assert_eq!(meta.tags.year, Some(2019));
}

#[test]
fn a_lossy_file_without_cover_is_still_imported_and_re_encoded() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let library = Library::open(dir.path().join("library")).unwrap();
    let wav = tone_wav(&ffmpeg, dir.path());
    let mp3 = dir.path().join("plain320.mp3");
    tagged_mp3(&ffmpeg, &wav, None, "320k", &mp3);

    let report = import(&library, std::slice::from_ref(&mp3), &optimizing());
    assert_eq!(report.imported, 1, "failures: {:?}", report.failures);
    let item = library.item(&report.created[0]).unwrap().unwrap();
    assert_eq!(item.ext, "opus");
    assert_eq!(item.thumb_state, ThumbState::None);
    let meta = library.audio_meta(&item.id).unwrap().unwrap();
    assert!(!meta.has_cover);
    assert_eq!(meta.tags.album.as_deref(), Some("Record"));
}

#[test]
fn a_low_bitrate_mp3_is_kept_byte_for_byte() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let library = Library::open(dir.path().join("library")).unwrap();
    let wav = tone_wav(&ffmpeg, dir.path());
    let mp3 = dir.path().join("low128.mp3");
    tagged_mp3(&ffmpeg, &wav, None, "128k", &mp3);
    let original = std::fs::read(&mp3).unwrap();

    let report = import(&library, std::slice::from_ref(&mp3), &optimizing());
    let item = library.item(&report.created[0]).unwrap().unwrap();
    assert_eq!(item.ext, "mp3");
    assert_eq!(item.opt_state, OptState::Skipped);
    assert_eq!(item.bytes, original.len() as u64);
}

#[test]
fn without_the_optimizer_music_is_stored_as_it_arrived() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let library = Library::open(dir.path().join("library")).unwrap();
    let wav = tone_wav(&ffmpeg, dir.path());

    let report = import(
        &library,
        std::slice::from_ref(&wav),
        &ImportOptions::default(),
    );
    let item = library.item(&report.created[0]).unwrap().unwrap();
    assert_eq!(item.kind, Kind::Audio);
    assert_eq!(item.ext, "wav");
    assert_eq!(item.opt_state, OptState::Skipped);
    assert!(library.audio_meta(&item.id).unwrap().is_some());
}

/// A tagged 128 kbps MP3 (kept as it is, so the tags are what was written).
fn tagged_track(ffmpeg: &Path, wav: &Path, out: &Path, tags: &[(&str, &str)]) {
    let mut args = vec!["-i".to_string(), wav.to_str().unwrap().to_string()];
    args.extend(["-c:a", "libmp3lame", "-b:a", "128k"].map(String::from));
    for (key, value) in tags {
        args.push("-metadata".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(out.to_str().unwrap().to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ffmpeg(ffmpeg, &args);
}

#[test]
fn music_is_listed_in_album_order_with_untagged_files_last() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let library = Library::open(dir.path().join("library")).unwrap();
    let wav = tone_wav(&ffmpeg, dir.path());
    let files = [
        ("z-untagged.mp3", vec![]),
        (
            "b2.mp3",
            vec![
                ("album", "Beta"),
                ("album_artist", "Zed"),
                ("track", "2/2"),
                ("title", "B two"),
            ],
        ),
        (
            "b1.mp3",
            vec![
                ("album", "Beta"),
                ("album_artist", "Zed"),
                ("track", "1/2"),
                ("title", "B one"),
            ],
        ),
        (
            "a-disc2.mp3",
            vec![
                ("album", "Alpha"),
                ("album_artist", "Ann"),
                ("disc", "2"),
                ("track", "1"),
                ("title", "A disc two"),
            ],
        ),
        (
            "a-disc1-t2.mp3",
            vec![
                ("album", "Alpha"),
                ("album_artist", "Ann"),
                ("disc", "1"),
                ("track", "2"),
                ("title", "A two"),
            ],
        ),
        (
            "a-disc1-t1.mp3",
            vec![
                ("album", "Alpha"),
                ("album_artist", "Ann"),
                ("disc", "1"),
                ("track", "1"),
                ("title", "A one"),
            ],
        ),
    ];
    let mut paths = Vec::new();
    for (name, tags) in &files {
        let path = dir.path().join(name);
        tagged_track(&ffmpeg, &wav, &path, tags);
        paths.push(path);
    }
    import(&library, &paths, &ImportOptions::default());

    let titles: Vec<String> = library
        .music()
        .unwrap()
        .into_iter()
        .map(|(item, audio)| {
            audio
                .and_then(|audio| audio.tags.title)
                .unwrap_or(item.original_name)
        })
        .collect();
    assert_eq!(
        titles,
        [
            "A one",
            "A two",
            "A disc two",
            "B one",
            "B two",
            "z-untagged.mp3"
        ]
    );
}
