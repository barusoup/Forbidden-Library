//! `.lrc` sidecars through the import path: a sidecar is kept only beside the
//! song it names, and the lyrics are stored with that song. Fixtures are made
//! with ffmpeg, and the tests are skipped when no ffmpeg is installed.

use archive_core::{ImportOptions, Item, Library, Tools, import_paths, parse_lyrics};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn ffmpeg() -> Option<PathBuf> {
    Tools::discover().ffmpeg
}

/// A one-second tone at `hz` as WAV, written to `path`. Each song needs its own
/// pitch: identical audio is one file to the importer.
fn tone(ffmpeg: &Path, path: &Path, hz: u32) {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={hz}:duration=1"))
        .args(["-c:a", "pcm_s16le"])
        .arg(path)
        .output()
        .expect("ffmpeg starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The catalogued item whose original file name is `name`.
fn named(library: &Library, items: &[String], name: &str) -> Option<Item> {
    items
        .iter()
        .filter_map(|id| library.item(id).ok().flatten())
        .find(|item| item.original_name == name)
}

#[test]
fn a_matching_sidecar_is_kept_and_the_rest_are_discarded() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let music = dir.path().join("album");
    std::fs::create_dir(&music).unwrap();
    tone(&ffmpeg, &music.join("01. Song.wav"), 440);
    tone(&ffmpeg, &music.join("02. Bare.wav"), 523);
    tone(&ffmpeg, &music.join("03. Plain.wav"), 659);
    std::fs::write(
        music.join("01. Song.lrc"),
        "[ar:Someone]\n[00:01.00]First line\n[00:03.50]Second line\n",
    )
    .unwrap();
    // A sidecar with no song beside it is not imported, and neither is one
    // with no timing, since it cannot be shown in sync.
    std::fs::write(music.join("orphan.lrc"), "[00:01.00]Nobody's\n").unwrap();
    std::fs::write(music.join("03. Plain.lrc"), "words with no timing\n").unwrap();

    let library = Library::open(dir.path().join("library")).unwrap();
    let report = import_paths(
        &library,
        std::slice::from_ref(&music),
        &ImportOptions::default(),
        &mut |_| {},
    );
    assert!(
        report.failures.is_empty(),
        "failures: {:?}",
        report.failures
    );
    assert_eq!(report.imported, 3);
    // The orphan is counted. The plain-text sidecar is matched by name, so it is
    // read and refused quietly, and its song simply has no lyrics.
    assert_eq!(report.skipped, 1);

    // Nothing was catalogued from a sidecar itself.
    for id in &report.created {
        let item = library.item(id).unwrap().unwrap();
        assert!(
            !item.original_name.ends_with(".lrc"),
            "{}",
            item.original_name
        );
    }

    let song = named(&library, &report.created, "01. Song.wav").expect("song imported");
    let text = library.lyrics(&song.id).unwrap().expect("song has lyrics");
    let lines = parse_lyrics(&text);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].at_ms, 3_500);
    assert_eq!(lines[1].text, "Second line");

    let bare = named(&library, &report.created, "02. Bare.wav").unwrap();
    assert_eq!(library.lyrics(&bare.id).unwrap(), None);
    let plain = named(&library, &report.created, "03. Plain.wav").unwrap();
    assert_eq!(library.lyrics(&plain.id).unwrap(), None);
}

#[test]
fn a_reimport_adds_a_sidecar_to_a_song_that_has_none() {
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let dir = TempDir::new().unwrap();
    let music = dir.path().join("album");
    std::fs::create_dir(&music).unwrap();
    tone(&ffmpeg, &music.join("01. Song.wav"), 440);

    let library = Library::open(dir.path().join("library")).unwrap();
    let first = import_paths(
        &library,
        std::slice::from_ref(&music),
        &ImportOptions::default(),
        &mut |_| {},
    );
    let song = named(&library, &first.created, "01. Song.wav").unwrap();
    assert_eq!(library.lyrics(&song.id).unwrap(), None);

    // The sidecar arrives later, and the same folder is dropped again.
    std::fs::write(music.join("01. Song.lrc"), "[00:02.00]Late line\n").unwrap();
    let again = import_paths(
        &library,
        std::slice::from_ref(&music),
        &ImportOptions::default(),
        &mut |_| {},
    );
    assert_eq!(again.imported, 0);
    assert_eq!(again.duplicates, 1);
    let text = library
        .lyrics(&song.id)
        .unwrap()
        .expect("lyrics attached on re-import");
    assert_eq!(parse_lyrics(&text)[0].text, "Late line");
}
