//! A library under a password: sealing it, opening it, changing the password,
//! removing it, finishing an interrupted conversion, and backing it up.

use archive_core::{Encryption, ImportOptions, Library, TimelineFilter, import_paths};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";
const OTHER: &str = "another long one";

fn write_jpeg(path: &Path, seed: u8) {
    let mut image = RgbImage::new(96, 64);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgb([(x as u8).wrapping_mul(seed), (y as u8) ^ seed, seed]);
    }
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 90);
    encoder
        .encode_image(&DynamicImage::ImageRgb8(image))
        .unwrap();
}

/// A plain library with three photos imported. Returns it and the item count.
fn plain_library(root: &Path, sources: &Path) -> Library {
    let mut paths = Vec::new();
    for (index, name) in ["a.jpg", "b.jpg", "c.jpg"].iter().enumerate() {
        let path: PathBuf = sources.join(name);
        write_jpeg(&path, 17 + index as u8 * 61);
        paths.push(path);
    }
    let library = Library::open(root).unwrap();
    import_paths(&library, &paths, &ImportOptions::default(), &mut |_| {});
    assert_eq!(library.encryption(), Encryption::Plain);
    library
}

fn photo_count(library: &Library) -> usize {
    library.item_count(&TimelineFilter::default()).unwrap()
}

/// Every file under `store/` and `thumbs/`, as absolute paths.
fn library_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("store"), root.join("thumbs")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn is_sealed(path: &Path) -> bool {
    std::fs::read(path).unwrap().starts_with(b"FLV1")
}

fn is_plain_database(root: &Path) -> bool {
    std::fs::read(root.join("library.db"))
        .unwrap()
        .starts_with(b"SQLite format 3\0")
}

#[test]
fn a_password_seals_the_catalogue_and_every_file() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    let library = plain_library(&root, &sources);
    library.set_password(PASSWORD, &mut |_| {}).unwrap();

    assert_eq!(library.encryption(), Encryption::Open);
    assert_eq!(photo_count(&library), 3);
    assert!(
        library
            .load_preview_rgba(&library.items(&TimelineFilter::default()).unwrap()[0], 256)
            .is_ok()
    );
    assert!(!is_plain_database(&root), "the catalogue is sealed");
    let database = std::fs::read(root.join("library.db")).unwrap();
    assert!(
        !database.windows(5).any(|window| window == b"a.jpg"),
        "the catalogue holds no readable file names"
    );
    let files = library_files(&root);
    assert!(!files.is_empty());
    for path in &files {
        assert!(is_sealed(path), "{} is sealed", path.display());
    }
    assert!(root.join(archive_core::applock::KEY_FILE).is_file());
}

#[test]
fn a_sealed_library_stays_closed_until_its_password_opens_it() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    {
        let library = plain_library(&root, &sources);
        library.set_password(PASSWORD, &mut |_| {}).unwrap();
    }

    let library = Library::open(&root).unwrap();
    assert_eq!(library.encryption(), Encryption::Locked);
    assert!(!library.unlock_library("the wrong password").unwrap());
    assert_eq!(library.encryption(), Encryption::Locked);
    assert!(library.unlock_library(PASSWORD).unwrap());
    assert_eq!(library.encryption(), Encryption::Open);
    assert_eq!(photo_count(&library), 3);
    let item = library.items(&TimelineFilter::default()).unwrap()[0].clone();
    assert!(library.load_preview_rgba(&item, 256).is_ok());
}

#[test]
fn changing_the_password_rewraps_the_key_without_touching_the_files() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    let library = plain_library(&root, &sources);
    library.set_password(PASSWORD, &mut |_| {}).unwrap();
    let file = library_files(&root)[0].clone();
    let sealed_before = std::fs::read(&file).unwrap();
    let key_before = std::fs::read(root.join(archive_core::applock::KEY_FILE)).unwrap();

    assert!(!library.change_password("not the password", OTHER).unwrap());
    assert!(library.change_password(PASSWORD, OTHER).unwrap());

    assert_eq!(
        std::fs::read(&file).unwrap(),
        sealed_before,
        "no file was re-encrypted"
    );
    assert_ne!(
        std::fs::read(root.join(archive_core::applock::KEY_FILE)).unwrap(),
        key_before,
        "the key file holds the new wrapping"
    );
    drop(library);
    let reopened = Library::open(&root).unwrap();
    assert!(!reopened.unlock_library(PASSWORD).unwrap());
    assert!(reopened.unlock_library(OTHER).unwrap());
    assert_eq!(photo_count(&reopened), 3);
}

#[test]
fn removing_the_password_converts_the_library_back_to_plain() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    let library = plain_library(&root, &sources);
    library.set_password(PASSWORD, &mut |_| {}).unwrap();

    assert!(
        !library
            .remove_password("not the password", &mut |_| {})
            .unwrap()
    );
    assert_eq!(library.encryption(), Encryption::Open);
    assert!(library.remove_password(PASSWORD, &mut |_| {}).unwrap());

    assert_eq!(library.encryption(), Encryption::Plain);
    assert!(is_plain_database(&root));
    for path in library_files(&root) {
        assert!(!is_sealed(&path), "{} is plain again", path.display());
    }
    assert!(!root.join(archive_core::applock::KEY_FILE).exists());
    assert_eq!(photo_count(&library), 3);
    drop(library);
    let reopened = Library::open(&root).unwrap();
    assert_eq!(reopened.encryption(), Encryption::Plain);
    assert_eq!(photo_count(&reopened), 3);
}

#[test]
fn an_interrupted_conversion_is_finished_on_the_next_unlock() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    let library = plain_library(&root, &sources);
    let plain_copy = library_files(&root)[0].clone();
    let plain_bytes = std::fs::read(&plain_copy).unwrap();
    library.set_password(PASSWORD, &mut |_| {}).unwrap();
    drop(library);

    // Simulate a crash partway through: one file is back to plaintext, and the
    // key file still says the conversion is not finished.
    std::fs::write(&plain_copy, &plain_bytes).unwrap();
    let mut key = archive_core::applock::read(&root).unwrap().unwrap();
    key.state = archive_core::applock::KeyState::Encrypting;
    archive_core::applock::write(&root, &key).unwrap();

    let library = Library::open(&root).unwrap();
    assert_eq!(library.encryption(), Encryption::Locked);
    assert!(library.unlock_library(PASSWORD).unwrap());
    assert_eq!(library.encryption(), Encryption::Open);
    for path in library_files(&root) {
        assert!(
            is_sealed(&path),
            "{} was sealed by the resumed conversion",
            path.display()
        );
    }
    assert_eq!(
        archive_core::applock::read(&root).unwrap().unwrap().state,
        archive_core::applock::KeyState::Sealed
    );
    assert_eq!(photo_count(&library), 3);
}

#[test]
fn a_backup_of_a_sealed_library_opens_with_the_same_password() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    let library = plain_library(&root, &sources);
    library.set_password(PASSWORD, &mut |_| {}).unwrap();

    let destination = temp.path().join("backups");
    let report = library.backup(&destination).unwrap();

    let backup = Library::open(&report.path).unwrap();
    assert_eq!(backup.encryption(), Encryption::Locked);
    assert!(backup.unlock_library(PASSWORD).unwrap());
    assert_eq!(photo_count(&backup), 3);
    let database = std::fs::read(Path::new(&report.path).join("library.db")).unwrap();
    assert!(
        !database.starts_with(b"SQLite format 3\0"),
        "the backup's catalogue is sealed"
    );
}

#[test]
fn a_locked_library_is_not_backed_up_as_plaintext() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let root = temp.path().join("library");
    {
        let library = plain_library(&root, &sources);
        library.set_password(PASSWORD, &mut |_| {}).unwrap();
    }
    let locked = Library::open(&root).unwrap();
    assert!(locked.backup(temp.path().join("backups")).is_err());
}
