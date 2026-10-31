//! Compare post-unlock preview warmup with encrypted caches and with the
//! cache-free seals used by older versions. Only a throwaway library is used.
//! Run: cargo run --release -p archive-core --example unlockbench

use archive_core::{ImportOptions, Library, Paths, TimelineFilter, import_paths};
use image::codecs::jpeg::JpegEncoder;
use std::time::Instant;

fn warm(library: &Library, items: &[archive_core::Item]) {
    // Match the UI's four texture workers; include grid and viewer decoding.
    std::thread::scope(|scope| {
        for chunk in items.chunks(items.len().div_ceil(4)) {
            scope.spawn(move || {
                for item in chunk {
                    library.load_thumb_rgba(item).unwrap();
                    library.load_preview_rgba(item, 2048).unwrap();
                }
            });
        }
    });
}

fn main() {
    let temp = tempfile::tempdir().unwrap();
    let mut sources = Vec::new();
    for index in 0..12u32 {
        let path = temp.path().join(format!("photo-{index}.jpg"));
        let photo = image::RgbImage::from_fn(3000, 2000, |x, y| {
            image::Rgb([
                (x.wrapping_mul(13) ^ y ^ index.wrapping_mul(61)) as u8,
                (y.wrapping_mul(7) ^ x ^ index.wrapping_mul(17)) as u8,
                (x ^ y.wrapping_mul(3) ^ index.wrapping_mul(31)) as u8,
            ])
        });
        JpegEncoder::new_with_quality(
            std::io::BufWriter::new(std::fs::File::create(&path).unwrap()),
            90,
        )
        .encode_image(&photo)
        .unwrap();
        sources.push(path);
    }
    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_paths(&library, &sources, &ImportOptions::default(), &mut |_| {});
    assert_eq!(report.imported, 12);
    let items = library.items(&TimelineFilter::default()).unwrap();
    let collection = library.create_collection("Benchmark").unwrap();
    let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
    library.add_to_collection(collection, &ids).unwrap();
    library
        .create_lock(collection, "benchmark password")
        .unwrap();
    let filter = TimelineFilter {
        collection_id: Some(collection),
        ..Default::default()
    };
    for cached in [true, false] {
        let start = Instant::now();
        library.unlock(collection, "benchmark password").unwrap();
        let unlocked = start.elapsed();
        let items = library.items(&filter).unwrap();
        let start_warm = Instant::now();
        warm(&library, &items);
        println!(
            "cached={cached}: unlock={unlocked:.2?}, preview warmup={:.2?}, total={:.2?}",
            start_warm.elapsed(),
            start.elapsed()
        );
        if cached {
            // Reproduce legacy seals: plaintext caches were purged without
            // being saved, leaving the next unlock to regenerate them.
            for item in &items {
                for relative in Paths::preview_rel_paths(&item.id) {
                    let path = library.paths().abs(&relative);
                    if path.exists() {
                        std::fs::remove_file(path).unwrap();
                    }
                }
            }
            library.lock(collection).unwrap();
        }
    }
}
