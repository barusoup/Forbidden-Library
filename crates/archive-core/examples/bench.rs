//! Timing harness for the engine's hot paths.
//!
//!   cargo run --release -p archive-core --example bench
//!
//! Builds a throwaway library of synthetic photos, then reports how long
//! importing, re-importing (dedupe), reading the catalogue, optimizing and
//! verifying take, plus a large vector scan. Numbers are meant to be compared
//! between revisions on the same machine, not quoted as absolutes.

use archive_core::{
    ImportOptions, Library, OptimizeConfig, TimelineFilter, VectorIndex, human_bytes, import_paths,
};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use std::path::Path;
use std::time::Instant;

const PHOTOS: usize = 12;
const PHOTO_WIDTH: u32 = 3000;
const PHOTO_HEIGHT: u32 = 2000;
const SCAN_ITEMS: usize = 120_000;
const SCAN_DIMS: usize = archive_core::EMBED_DIMS;

fn main() {
    if std::env::var_os("FORBIDDEN_LIBRARY_FFMPEG_DIR").is_none()
        && Path::new("assets/ffmpeg/ffmpeg.exe").is_file()
    {
        // SAFETY: set before any worker thread reads the environment.
        unsafe {
            std::env::set_var("FORBIDDEN_LIBRARY_FFMPEG_DIR", "assets/ffmpeg");
        }
    }

    let root = std::env::temp_dir().join(format!("forbiddenlibrary-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();

    println!("building {PHOTOS} photos at {PHOTO_WIDTH}x{PHOTO_HEIGHT}...");
    let mut bytes = 0u64;
    for index in 0..PHOTOS {
        let path = media.join(format!("photo-{index:02}.jpg"));
        write_jpeg(&path, &photo(index as u32));
        bytes += std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    }
    println!("  {} of source media", human_bytes(bytes));

    let library = Library::open(root.join("library")).unwrap();

    // Images are optimized as they are copied, so this import includes that work.
    let options = ImportOptions {
        optimize: Some(OptimizeConfig::default()),
        ..Default::default()
    };
    let start = Instant::now();
    let report = import_paths(
        &library,
        std::slice::from_ref(&media),
        &options,
        &mut |_| {},
    );
    let import_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "import+optimize:{:8.1} ms  {:6.1} ms/photo  ({} imported, {} failed)",
        import_ms,
        import_ms / report.imported.max(1) as f64,
        report.imported,
        report.failed
    );

    let start = Instant::now();
    let again = import_paths(&library, &[media], &options, &mut |_| {});
    let dedupe_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "re-import:       {:8.1} ms  {:6.1} ms/photo  ({} duplicates)",
        dedupe_ms,
        dedupe_ms / again.duplicates.max(1) as f64,
        again.duplicates
    );

    let start = Instant::now();
    let rounds = 20;
    for _ in 0..rounds {
        let items = library.items(&TimelineFilter::default()).unwrap();
        let stats = library.stats().unwrap();
        std::hint::black_box((items.len(), stats.photos));
    }
    let read_ms = start.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
    println!(
        "catalogue read:  {:8.2} ms  (items + stats, per reload)",
        read_ms
    );

    let start = Instant::now();
    let report = library
        .verify(archive_core::VerifyOptions::default())
        .unwrap();
    let verify_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "verify:          {:8.1} ms  {:6.1} ms/file  ({} ok, {} corrupt)",
        verify_ms,
        verify_ms / report.checked.max(1) as f64,
        report.ok,
        report.corrupt.len()
    );

    bench_scan();

    let _ = std::fs::remove_dir_all(&root);
}

/// The stored-vector layout the app loads at startup, then a full scan.
fn bench_scan() {
    println!("building {SCAN_ITEMS} x {SCAN_DIMS} vectors...");
    let mut seed = 0x2545_f491u32;
    let mut next = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / 16_777_216.0 - 0.5
    };
    let mut ids = Vec::with_capacity(SCAN_ITEMS);
    let mut vectors = Vec::with_capacity(SCAN_ITEMS * SCAN_DIMS);
    for item in 0..SCAN_ITEMS {
        ids.push(format!("{item:06}"));
        for _ in 0..SCAN_DIMS {
            vectors.push(next());
        }
    }
    let start = Instant::now();
    let index = VectorIndex::from_flat(SCAN_DIMS, ids, vectors);
    println!(
        "index build:     {:8.1} ms  ({} items)",
        start.elapsed().as_secs_f64() * 1000.0,
        index.len()
    );
    let query: Vec<f32> = (0..SCAN_DIMS).map(|_| next()).collect();
    let baseline: Vec<f32> = (0..SCAN_DIMS).map(|_| next()).collect();
    let rounds = 10;
    let start = Instant::now();
    for _ in 0..rounds {
        let hits = index.search_relative(&query, &baseline, 300, -1.0);
        std::hint::black_box(hits);
    }
    let scan_ms = start.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
    println!(
        "vector scan:     {:8.2} ms  (relative top-300 over {} items)",
        scan_ms,
        index.len()
    );
}

fn photo(seed: u32) -> RgbImage {
    let mut image = RgbImage::new(PHOTO_WIDTH, PHOTO_HEIGHT);
    let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
    for y in 0..PHOTO_HEIGHT {
        for x in 0..PHOTO_WIDTH {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (state >> 24) as u8 / 10;
            let r = ((x * 255 / PHOTO_WIDTH) as u8).wrapping_add(noise);
            let g = ((y * 255 / PHOTO_HEIGHT) as u8).wrapping_add(noise / 2);
            let b = (((x + y) * 255 / (PHOTO_WIDTH + PHOTO_HEIGHT)) as u8).wrapping_add(noise / 3);
            image.put_pixel(x, y, Rgb([r, g, b]));
        }
    }
    image
}

fn write_jpeg(path: &Path, image: &RgbImage) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 92);
    encoder
        .encode_image(&DynamicImage::ImageRgb8(image.clone()))
        .unwrap();
}
