//! Importing into a library that is sealed from the start: photos (through
//! the optimizer), previews, verification and a video. Every read that used
//! to go straight to a file must work through the sealing.

use archive_core::{
    ImportOptions, Library, OptimizeConfig, TimelineFilter, VerifyOptions, import_paths,
    playback_info,
};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";

fn write_jpeg(path: &Path, seed: u8) {
    let mut image = RgbImage::new(160, 120);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgb([(x as u8).wrapping_mul(seed), (y as u8) ^ seed, seed]);
    }
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 90);
    encoder
        .encode_image(&DynamicImage::ImageRgb8(image))
        .unwrap();
}

fn write_png(path: &Path, seed: u8) {
    let mut image = RgbImage::new(120, 90);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgb([seed, (x as u8) ^ (y as u8), (x as u8).wrapping_add(seed)]);
    }
    DynamicImage::ImageRgb8(image).save(path).unwrap();
}

/// A sealed library with nothing in it yet.
fn sealed_library(root: &Path) -> Library {
    let library = Library::open(root).unwrap();
    library.set_password(PASSWORD, &mut |_| {}).unwrap();
    library
}

fn every_file_sealed(root: &Path) -> bool {
    ["store", "thumbs"].iter().all(|dir| {
        walk(&root.join(dir))
            .iter()
            .all(|path| std::fs::read(path).unwrap().starts_with(b"FLV1"))
    })
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
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
    found
}

#[test]
fn a_sealed_library_imports_optimizes_previews_and_verifies_its_photos() {
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let jpeg_a = sources.join("a.jpg");
    let jpeg_b = sources.join("b.jpg");
    let png = sources.join("c.png");
    write_jpeg(&jpeg_a, 23);
    write_jpeg(&jpeg_b, 99);
    write_png(&png, 7);

    let library = sealed_library(&temp.path().join("library"));
    let options = ImportOptions {
        optimize: Some(OptimizeConfig::default()),
        ..ImportOptions::default()
    };
    import_paths(&library, &[jpeg_a, jpeg_b, png], &options, &mut |_| {});

    let items = library.items(&TimelineFilter::default()).unwrap();
    assert_eq!(items.len(), 3);
    for item in &items {
        let (width, height, rgba) = library.load_full_rgba(item, 0).unwrap();
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert!(library.load_preview_rgba(item, 2048).is_ok());
        assert!(
            library.embedding_frame(item).is_ok(),
            "the vision frame reads back"
        );
        let original = library.plain_source(item).unwrap();
        assert!(original.path().is_file());
    }
    let report = library
        .verify(VerifyOptions {
            hashes: true,
            thumbs: true,
            orphans: true,
        })
        .unwrap();
    assert_eq!(
        report.corrupt.len(),
        0,
        "every sealed file hashes to its catalogue checksum"
    );
    assert_eq!(report.missing.len(), 0);
    assert!(every_file_sealed(library.root()));
}

#[test]
fn a_sealed_library_plays_and_previews_a_video() {
    let tools = archive_core::Tools::discover();
    let Some(ffmpeg) = tools.ffmpeg.as_deref() else {
        eprintln!("skipped: ffmpeg is not available");
        return;
    };
    let temp = TempDir::new().unwrap();
    let sources = temp.path().join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    let clip = sources.join("clip.mp4");
    let made = Command::new(ffmpeg)
        .args([
            "-y",
            "-loglevel",
            "error",
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
            "mpeg4",
            "-c:a",
            "aac",
        ])
        .arg(&clip)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skipped: ffmpeg could not write the sample clip");
        return;
    }

    let library = sealed_library(&temp.path().join("library"));
    import_paths(
        &library,
        std::slice::from_ref(&clip),
        &ImportOptions::default(),
        &mut |_| {},
    );
    let items = library.items(&TimelineFilter::default()).unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];

    // ffmpeg reads a decrypted copy, which is gone once the playback is dropped.
    let original = library.plain_source(item).unwrap();
    let info = playback_info(&tools, original.path()).unwrap();
    assert!(info.width > 0 && info.height > 0);
    drop(original);
    assert!(
        library.load_preview_rgba(item, 2048).is_ok(),
        "the poster frame decodes"
    );
    assert!(every_file_sealed(library.root()));
    assert!(
        walk(&library.root().join("tmp")).is_empty(),
        "no decrypted copies are left behind"
    );
}
