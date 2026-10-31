//! Stills the Rust decoders cannot read are decoded by ffmpeg. This checks
//! that the path to the screen keeps every sample: ffmpeg's output is a
//! lossless PNG, so nothing is lost on the way to a proxy or a full view.

use archive_core::{ImportOptions, Library, TimelineFilter, Tools, import_paths};
use image::{Rgb, RgbImage};
use std::path::Path;
use std::process::Command;
use std::sync::Once;
use tempfile::TempDir;

/// Point the engine at the bundled ffmpeg for this process, when it is
/// present. Discovery is cached, so this runs before any test discovers tools.
fn point_at_bundled_ffmpeg() {
    static POINT: Once = Once::new();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/ffmpeg");
    if !dir.join("ffmpeg.exe").is_file() {
        return;
    }
    POINT.call_once(|| {
        // SAFETY: set before this process starts any engine work, because every
        // test calls this first and `Once` makes the others wait for it.
        unsafe {
            std::env::set_var("FORBIDDEN_LIBRARY_FFMPEG_DIR", &dir);
        }
    });
}

/// A picture with edges, gradients and colours the AVIF encoder has to
/// represent exactly, so a lossless round trip is a fair test.
fn picture(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        Rgb([
            (x * 7 % 256) as u8,
            (y * 11 % 256) as u8,
            ((x ^ y) * 3 % 256) as u8,
        ])
    })
}

#[test]
fn a_lossless_still_the_rust_decoders_cannot_read_comes_back_exactly() {
    point_at_bundled_ffmpeg();
    let Some(ffmpeg) = Tools::discover().ffmpeg else {
        eprintln!("skipping: ffmpeg was not found");
        return;
    };

    let temp = TempDir::new().unwrap();
    let source_png = temp.path().join("source.png");
    let source = picture(96, 64);
    source.save(&source_png).unwrap();

    // Lossless AV1 in RGB planes: the decoded picture must equal the source.
    // A missing AV1 encoder is an environment fact, not a failure.
    let avif = temp.path().join("still.avif");
    let encoded = Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&source_png)
        .args([
            "-c:v",
            "libaom-av1",
            "-still-picture",
            "1",
            "-crf",
            "0",
            "-pix_fmt",
            "gbrp",
            "-frames:v",
            "1",
        ])
        .arg(&avif)
        .status()
        .unwrap();
    if !encoded.success() {
        eprintln!("skipping: this ffmpeg cannot write a lossless AVIF");
        return;
    }
    // The point of the test: the image crate has no AVIF decoder, so the
    // ffmpeg path is the only one that can show this file.
    assert!(image::open(&avif).is_err());

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_paths(
        &library,
        std::slice::from_ref(&avif),
        &ImportOptions::default(),
        &mut |_| {},
    );
    assert_eq!(report.imported, 1, "{report:?}");
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    assert_eq!((item.width, item.height), (Some(96), Some(64)));

    let (width, height, rgba) = library.load_full_rgba(&item, 0).unwrap();
    assert_eq!((width, height), (96, 64));
    let expected = image::DynamicImage::ImageRgb8(source).to_rgba8();
    assert_eq!(rgba, expected.into_raw(), "the full view lost samples");

    let (width, height, _) = library.load_preview_rgba(&item, 2048).unwrap();
    assert_eq!((width, height), (96, 64));
}
