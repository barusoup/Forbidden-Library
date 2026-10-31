use archive_core::{
    ImportEvent, ImportOptions, Library, OptState, OptimizeConfig, ThumbState, TimelineFilter,
    VerifyOptions, human_bytes, import_paths,
};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, Rgb, RgbImage};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::TempDir;

fn photo(width: u32, height: u32) -> RgbImage {
    let mut image = RgbImage::new(width, height);
    let mut seed = 0x1234_5678u32;
    for y in 0..height {
        for x in 0..width {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (seed >> 24) as u8 / 12;
            let r = ((x * 255 / width.max(1)) as u8).saturating_add(noise);
            let g = ((y * 255 / height.max(1)) as u8).saturating_add(noise);
            let b = (((x + y) * 255 / (width + height).max(1)) as u8).saturating_add(noise);
            image.put_pixel(x, y, Rgb([r, g, b]));
        }
    }
    image
}

fn write_png(path: &Path, image: &RgbImage) {
    let file = std::fs::File::create(path).unwrap();
    // Deliberately poor first-pass compression: the archive should fix it.
    let encoder = PngEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        CompressionType::Fast,
        FilterType::NoFilter,
    );
    encoder
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
}

fn write_jpeg(path: &Path, image: &RgbImage) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 92);
    encoder
        .encode_image(&image::DynamicImage::ImageRgb8(image.clone()))
        .unwrap();
}

fn import_all(library: &Library, paths: &[std::path::PathBuf]) -> archive_core::ImportReport {
    let mut events = Vec::new();
    let report = import_paths(library, paths, &ImportOptions::default(), &mut |event| {
        events.push(event)
    });
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ImportEvent::Finished(_))),
        "import must always finish with a report"
    );
    report
}

#[test]
fn import_deduplicates_and_builds_previews() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let png = source.join("sunset.png");
    let jpg = source.join("harbour.jpg");
    write_png(&png, &photo(320, 200));
    write_jpeg(&jpg, &photo(300, 400));

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_all(&library, &[png.clone(), jpg.clone()]);
    assert_eq!(report.imported, 2, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");

    let items = library.items(&TimelineFilter::default()).unwrap();
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(item.kind, archive_core::Kind::Image);
        assert!(item.width.unwrap() > 0);
        assert!(item.height.unwrap() > 0);
        assert_eq!(item.thumb_state, ThumbState::Ready);
        assert!(item.avg_color > 0);
        assert!(library.paths().abs(&item.rel_path).is_file());
        assert!(library.paths().thumb_path(&item.id).is_file());
        let (width, height, pixels) = library.load_thumb_rgba(item).unwrap();
        assert!(width > 0 && height > 0);
        assert_eq!(pixels.len(), (width * height * 4) as usize);
    }

    let again = import_all(&library, std::slice::from_ref(&png));
    assert_eq!(again.imported, 0);
    assert_eq!(again.duplicates, 1);
    assert_eq!(library.items(&TimelineFilter::default()).unwrap().len(), 2);

    let stats = library.stats().unwrap();
    assert_eq!(stats.photos, 2);
    assert_eq!(stats.pending_thumbs, 0);
    assert_eq!(stats.missing_files, 0);
    assert_eq!(stats.missing_thumbs, 0);
}

/// One import call larger than a commit batch must still commit every row,
/// and two files with identical bytes inside that call must be one item and
/// one duplicate, not two rows with the same id.
#[test]
fn a_batch_import_commits_every_row_and_absorbs_a_duplicate() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    // More files than one catalogue batch, so the last partial batch and the
    // full ones are both exercised.
    let count = 70;
    let mut paths = Vec::new();
    for index in 0..count {
        let path = source.join(format!("note-{index}.txt"));
        std::fs::write(&path, format!("note {index}\n")).unwrap();
        paths.push(path);
    }
    let twin = source.join("twin.txt");
    std::fs::write(&twin, b"note 0\n").unwrap();
    paths.push(twin);

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_paths(
        &library,
        &paths,
        &ImportOptions {
            include_other: true,
            cancel: None,
            optimize: None,
        },
        &mut |_| {},
    );
    assert_eq!(report.imported, count, "{report:?}");
    assert_eq!(report.duplicates, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(
        library.item_count(&TimelineFilter::default()).unwrap(),
        count,
        "every written row must be readable after the import returns"
    );
    for item in library.items(&TimelineFilter::default()).unwrap() {
        assert!(library.paths().abs(&item.rel_path).is_file());
    }
}

#[test]
fn embeddings_are_stored_ranked_and_removed_with_the_item() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let first = source.join("first.png");
    let second = source.join("second.png");
    write_png(&first, &photo(320, 200));
    write_png(&second, &photo(200, 320));

    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[first, second]);
    let items = library.items(&TimelineFilter::default()).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        library
            .pending_embeddings(archive_core::EMBED_MODEL, 10)
            .unwrap()
            .len(),
        2
    );

    // A preview is what the vision encoder uploads; it must exist for both.
    for item in &items {
        let frame = library.embedding_frame(item).unwrap();
        assert!(frame.starts_with(&[0xFF, 0xD8]), "expected a JPEG preview");
    }

    let mut warm = vec![0.0; archive_core::EMBED_DIMS];
    warm[0] = 1.0;
    let mut cool = vec![0.0; archive_core::EMBED_DIMS];
    cool[1] = 1.0;
    library
        .set_embedding(&items[0].id, archive_core::EMBED_MODEL, &warm)
        .unwrap();
    library
        .set_embedding(&items[1].id, archive_core::EMBED_MODEL, &cool)
        .unwrap();
    assert!(
        library
            .pending_embeddings(archive_core::EMBED_MODEL, 10)
            .unwrap()
            .is_empty()
    );

    let rows = library
        .embeddings(archive_core::EMBED_MODEL, archive_core::EMBED_DIMS)
        .unwrap();
    assert_eq!(rows.len(), 2);
    let index = archive_core::VectorIndex::from_rows(archive_core::EMBED_DIMS, rows);
    let hits = index.search(&warm, 5, 0.0);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0, items[0].id);
    assert!(hits[0].1 > hits[1].1);

    // Permanently deleting an item takes its vector with it.
    library.delete_forever(&[items[1].id.clone()]).unwrap();
    let rows = library
        .embeddings(archive_core::EMBED_MODEL, archive_core::EMBED_DIMS)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, items[0].id);
}

#[test]
fn folder_import_skips_non_media() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("drop");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    write_png(&source.join("a.png"), &photo(64, 64));
    write_jpeg(&source.join("nested").join("b.jpg"), &photo(64, 64));
    std::fs::write(source.join("notes.txt"), "not media").unwrap();
    std::fs::write(source.join("empty.png"), "").unwrap();

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_all(&library, &[source]);
    assert_eq!(report.imported, 2, "{report:?}");
    assert_eq!(library.items(&TimelineFilter::default()).unwrap().len(), 2);
}

#[test]
fn corrupt_media_is_refused_without_a_trace() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("drop");
    std::fs::create_dir_all(&source).unwrap();
    write_png(&source.join("good.png"), &photo(64, 64));
    // Looks like a JPEG by name and by its opening bytes, but cannot be decoded.
    let mut corrupt = vec![0xFF, 0xD8, 0xFF, 0xE0];
    corrupt.extend(std::iter::repeat_n(0x5Au8, 4096));
    std::fs::write(source.join("broken.jpg"), corrupt).unwrap();

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_all(&library, &[source]);
    assert_eq!(report.imported, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    // The refusal is reported, with its reason, rather than dropped silently.
    assert_eq!(report.failures.len(), 1, "{report:?}");
    assert!(
        report.failures[0]
            .1
            .contains("could not be decoded, so it was not imported"),
        "{report:?}"
    );
    assert_eq!(library.items(&TimelineFilter::default()).unwrap().len(), 1);
    let stored = walkdir::WalkDir::new(library.paths().store())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();
    assert_eq!(stored, 1, "the corrupt copy was left in the store");
}

#[test]
fn trash_restore_and_delete_forever_are_safe() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("keep.png");
    write_png(&png, &photo(96, 96));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[png]);
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    let stored = library.paths().abs(&item.rel_path);

    assert_eq!(library.trash(std::slice::from_ref(&item.id)).unwrap(), 1);
    assert_eq!(
        library
            .items(&TimelineFilter {
                trashed: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
    assert!(stored.is_file(), "trashing must not delete the file");

    library.restore(std::slice::from_ref(&item.id)).unwrap();
    assert_eq!(library.items(&TimelineFilter::default()).unwrap().len(), 1);

    library.trash(std::slice::from_ref(&item.id)).unwrap();
    assert_eq!(library.empty_trash().unwrap(), 1);
    assert_eq!(library.items(&TimelineFilter::default()).unwrap().len(), 0);
    assert!(
        !stored.exists(),
        "emptying the trash removes the stored file"
    );
    assert!(!library.paths().thumb_path(&item.id).exists());
}

#[test]
fn the_trash_lists_the_newest_deletion_first() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("first.png");
    let second = temp.path().join("second.png");
    write_png(&first, &photo(64, 64));
    write_png(&second, &photo(80, 80));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[first, second]);
    let mut items = library.items(&TimelineFilter::default()).unwrap();
    assert_eq!(items.len(), 2);
    let older = items.remove(0);
    let newer = items.remove(0);

    library.trash(std::slice::from_ref(&older.id)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    library.trash(std::slice::from_ref(&newer.id)).unwrap();

    let trashed = library
        .items(&TimelineFilter {
            trashed: true,
            ..Default::default()
        })
        .unwrap();
    let ids: Vec<&str> = trashed.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![newer.id.as_str(), older.id.as_str()],
        "the most recently deleted item comes first"
    );
}

#[test]
fn verify_finds_tampering_and_missing_files() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("a.png");
    write_png(&source, &photo(128, 128));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[source]);
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    let stored = library.paths().abs(&item.rel_path);

    let report = library.verify(VerifyOptions::default()).unwrap();
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(report.ok, 1);

    let mut bytes = std::fs::read(&stored).unwrap();
    bytes.push(0);
    std::fs::write(&stored, &bytes).unwrap();
    let report = library.verify(VerifyOptions::default()).unwrap();
    assert_eq!(report.corrupt.len(), 1);
    assert!(!report.is_healthy());

    std::fs::remove_file(&stored).unwrap();
    let report = library.verify(VerifyOptions::default()).unwrap();
    assert_eq!(report.missing.len(), 1);
}

#[test]
fn opening_the_library_cleans_staging_but_keeps_the_store() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("a.png");
    write_png(&png, &photo(64, 64));
    let root = temp.path().join("library");
    let library = Library::open(&root).unwrap();
    import_all(&library, &[png]);
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    let stored = library.paths().abs(&item.rel_path);
    assert!(stored.is_file());

    let stale = library.paths().tmp().join("stale.stage");
    std::fs::write(&stale, b"leftover").unwrap();
    drop(library);

    let reopened = Library::open(&root).unwrap();
    assert!(!stale.exists(), "staging leftovers are removed on open");
    assert!(stored.is_file(), "stored files are never touched on open");
    assert_eq!(reopened.items(&TimelineFilter::default()).unwrap().len(), 1);
}

/// Imports with the optimizer on, as the app does by default.
fn import_optimized(library: &Library, paths: &[std::path::PathBuf]) -> archive_core::ImportReport {
    let options = ImportOptions {
        optimize: Some(OptimizeConfig::default()),
        ..Default::default()
    };
    import_paths(library, paths, &options, &mut |_| {})
}

#[test]
fn optimization_is_verified_and_final() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("large.png");
    write_png(&png, &photo(512, 512));
    let original_bytes = std::fs::metadata(&png).unwrap().len();
    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_optimized(&library, &[png]);
    assert_eq!(report.imported, 1, "{report:?}");

    // A deliberately poorly-compressed PNG must shrink before it is stored.
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    assert_eq!(item.opt_state, OptState::Optimized, "{:?}", item.opt_error);
    assert_eq!(item.original_bytes, original_bytes);
    assert!(
        item.bytes < original_bytes,
        "optimized file must be smaller"
    );
    assert!(library.paths().abs(&item.rel_path).is_file());
    assert!(
        !library.root().join("backups").exists(),
        "the replaced original is not kept"
    );

    let verify = library.verify(VerifyOptions::default()).unwrap();
    assert!(verify.is_healthy(), "{verify:?}");
}

#[test]
fn reimporting_an_optimized_original_is_a_duplicate() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("large.png");
    write_png(&png, &photo(512, 512));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_optimized(&library, std::slice::from_ref(&png));
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    assert_eq!(item.opt_state, OptState::Optimized, "{:?}", item.opt_error);

    // The original's bytes differ in size from what is stored, and must still
    // be recognised rather than archived a second time.
    let again = import_optimized(&library, &[png]);
    assert_eq!(again.duplicates, 1, "{again:?}");
    assert_eq!(again.imported, 0, "{again:?}");
    assert_eq!(library.items(&TimelineFilter::default()).unwrap().len(), 1);
}

#[test]
fn deleting_an_optimized_item_removes_its_files() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("large.png");
    write_png(&png, &photo(512, 512));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_optimized(&library, &[png]);
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    if item.opt_state != OptState::Optimized {
        return;
    }
    let optimized_path = library.paths().abs(&item.rel_path);
    library.trash(std::slice::from_ref(&item.id)).unwrap();
    assert_eq!(library.empty_trash().unwrap(), 1);
    assert!(!optimized_path.exists());
    let report = library.verify(VerifyOptions::default()).unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn collections_hold_items_without_owning_them() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let first = source.join("first.png");
    let second = source.join("second.png");
    write_png(&first, &photo(200, 200));
    write_png(&second, &photo(240, 160));

    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[first, second]);
    let items = library.items(&TimelineFilter::default()).unwrap();
    let a = items[0].id.clone();
    let b = items[1].id.clone();

    let all = library.create_collection("All").unwrap();
    let some = library.create_collection("Some").unwrap();
    library
        .add_to_collection(all, &[a.clone(), b.clone()])
        .unwrap();
    library
        .add_to_collection(some, std::slice::from_ref(&a))
        .unwrap();

    // "All" holds both, so it is the only one that already has the pair;
    // "Some" is still offered for the pair but skipped for `a` alone.
    assert_eq!(
        library
            .collections_containing_all(&[a.clone(), b.clone()])
            .unwrap(),
        vec![all]
    );
    assert_eq!(
        library
            .collections_containing_all(std::slice::from_ref(&a))
            .unwrap(),
        vec![all, some]
    );
    assert_eq!(
        library
            .collection_ids_for_items(std::slice::from_ref(&b))
            .unwrap(),
        vec![all]
    );

    assert_eq!(
        library
            .remove_from_collection(some, std::slice::from_ref(&a))
            .unwrap(),
        1
    );
    assert!(
        library.item(&a).unwrap().is_some(),
        "removing from a collection must not remove the item"
    );
    let collections = library.collections().unwrap();
    assert_eq!(
        collections
            .iter()
            .find(|collection| collection.id == some)
            .unwrap()
            .count,
        0
    );
    assert_eq!(
        collections
            .iter()
            .find(|collection| collection.id == all)
            .unwrap()
            .count,
        2
    );
}

/// The dominant photo path: a JPEG offered a visually-lossless WebP. It
/// needs ffmpeg, so it is skipped on a checkout that has not fetched it.
#[test]
fn byte_formatting_is_sentence_short() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(1023), "1023 B");
    assert_eq!(human_bytes(1024), "1 KB");
    assert_eq!(human_bytes(1024 * 1024 * 14 / 10), "1.4 MB");
    assert_eq!(human_bytes(1024 * 1024 * 1024 * 23), "23 GB");
}

#[test]
fn timeline_pages_cover_every_item_exactly_once() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let mut files = Vec::new();
    for index in 0..23u32 {
        let path = source.join(format!("photo-{index:02}.png"));
        write_png(&path, &photo(64 + index, 48 + index));
        files.push(path);
    }
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &files);

    let filter = TimelineFilter::default();
    let all = library.items(&filter).unwrap();
    assert_eq!(all.len(), 23);

    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = library.item_page(&filter, cursor.as_ref(), 7).unwrap();
        // Only the first page counts the catalogue; later pages leave the total
        // to the caller, which already has it.
        assert_eq!(page.total, cursor.is_none().then_some(23));
        if page.items.is_empty() {
            break;
        }
        seen.extend(page.items.iter().map(|item| item.id.clone()));
        cursor = page.cursor;
        if page.done {
            break;
        }
    }
    let expected: Vec<String> = all.iter().map(|item| item.id.clone()).collect();
    assert_eq!(seen, expected, "paging must visit every row in order, once");
    assert_eq!(library.item_ids(&filter).unwrap(), expected);
    assert_eq!(library.capture_index(&filter).unwrap().len(), 23);
    assert_eq!(library.item_count(&filter).unwrap(), 23);
}

#[test]
fn a_duplicate_import_is_detected_without_staging_a_copy() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("photo.png");
    write_png(&png, &photo(240, 160));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, std::slice::from_ref(&png));
    let tmp = library.paths().tmp().to_path_buf();
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);

    let report = import_all(&library, &[png]);
    assert_eq!(report.duplicates, 1, "{report:?}");
    assert_eq!(report.imported, 0, "{report:?}");
    assert_eq!(
        std::fs::read_dir(&tmp).unwrap().count(),
        0,
        "a duplicate must be recognised by hashing, without copying it into staging"
    );
}

#[test]
fn a_backup_is_a_complete_usable_library() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let first = source.join("first.png");
    let second = source.join("second.png");
    write_png(&first, &photo(200, 140));
    write_png(&second, &photo(260, 180));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[first, second]);

    let report = library.backup(temp.path().join("backups")).unwrap();
    let copied = Path::new(&report.path);
    assert!(copied.join("library.db").is_file());
    assert!(copied.join("backup.json").is_file());
    assert!(copied.join("thumbs").is_dir());
    assert!(report.items >= 2, "{report:?}");

    let restored = Library::open(copied).unwrap();
    restored
        .load_thumb_rgba(&restored.items(&TimelineFilter::default()).unwrap()[0])
        .unwrap();
    let verify = restored.verify(VerifyOptions::default()).unwrap();
    assert!(verify.is_healthy(), "{verify:?}");
    assert_eq!(restored.items(&TimelineFilter::default()).unwrap().len(), 2);

    assert!(
        library.backup(library.root().join("inside")).is_err(),
        "a backup inside the library would copy itself"
    );
}

#[test]
fn a_library_can_only_be_opened_once() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("library");
    let library = Library::open(&root).unwrap();
    let error = Library::open(&root)
        .err()
        .expect("a second owner must be refused");
    assert!(
        error.to_string().contains("already open"),
        "unexpected error: {error}"
    );
    drop(library);
    Library::open(&root).expect("the lock is released when the owner goes away");
}

#[test]
fn a_missing_preview_is_rebuilt_when_it_is_read() {
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("photo.png");
    write_png(&png, &photo(300, 200));
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &[png]);
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    assert_eq!(item.thumb_recipe, archive_core::THUMB_RECIPE);
    let thumb = library.paths().thumb_path(&item.id);
    std::fs::remove_file(&thumb).unwrap();
    let (width, height, pixels) = library.load_thumb_rgba(&item).unwrap();
    assert!(width > 0 && height > 0);
    assert_eq!(pixels.len(), (width * height * 4) as usize);
    assert!(thumb.is_file(), "the preview was rebuilt");
}

/// A real video goes through the bounded probe, the bounded poster extractor
/// and the format/protocol allowlist. If any of those flags were wrong, the
/// preview would be missing rather than merely slow.
#[test]
fn a_video_is_probed_and_previewed_with_bounded_ffmpeg() {
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/ffmpeg");
    if !tools.join("ffmpeg.exe").is_file() {
        eprintln!("skipping: assets/ffmpeg is not present");
        return;
    }
    // SAFETY: set before this test starts any worker that reads it.
    unsafe {
        std::env::set_var("FORBIDDEN_LIBRARY_FFMPEG_DIR", &tools);
    }
    let temp = TempDir::new().unwrap();
    let frame = temp.path().join("frame.png");
    write_png(&frame, &photo(320, 240));
    let video = temp.path().join("clip.mp4");
    let status = std::process::Command::new(tools.join("ffmpeg.exe"))
        .args(["-hide_banner", "-loglevel", "error", "-loop", "1", "-i"])
        .arg(&frame)
        .args(["-t", "1", "-pix_fmt", "yuv420p", "-y"])
        .arg(&video)
        .status()
        .expect("the bundled ffmpeg must start");
    assert!(status.success(), "could not build the video fixture");

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_all(&library, &[video]);
    assert_eq!(report.imported, 1, "{report:?}");
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    assert_eq!(item.kind, archive_core::Kind::Video);
    assert!(item.duration_ms.unwrap_or(0) > 0);
    assert_eq!(
        item.thumb_state,
        ThumbState::Ready,
        "preview failed: {:?}",
        item.thumb_error
    );
    assert!(library.paths().thumb_path(&item.id).is_file());
}

#[test]
fn a_cancelled_import_stops_early_and_names_what_landed() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let mut paths = Vec::new();
    for index in 0..2000 {
        let path = source.join(format!("note-{index}.txt"));
        std::fs::write(&path, format!("note {index}\n")).unwrap();
        paths.push(path);
    }

    let library = Library::open(temp.path().join("library")).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = ImportOptions {
        include_other: true,
        cancel: Some(Arc::clone(&cancel)),
        optimize: None,
    };
    let report = import_paths(&library, &paths, &options, &mut |event| {
        // Cancel once the first rows are committed.
        if matches!(event, ImportEvent::FileDone { .. }) {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(report.imported > 0, "{report:?}");
    assert!(
        report.imported < paths.len(),
        "cancelling must stop the copy early: {report:?}"
    );
    assert_eq!(report.created.len(), report.imported, "{report:?}");
    assert_eq!(
        library.item_count(&TimelineFilter::default()).unwrap(),
        report.imported,
        "only the rows that landed before the cancel exist"
    );
}

#[test]
fn an_import_is_taken_back_by_the_rows_it_created_and_no_others() {
    let temp = TempDir::new().unwrap();
    let library = Library::open(temp.path().join("library")).unwrap();
    let existing = temp.path().join("existing.png");
    write_png(&existing, &photo(96, 96));
    import_all(&library, std::slice::from_ref(&existing));
    let kept = library.items(&TimelineFilter::default()).unwrap().remove(0);

    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let mut paths = Vec::new();
    for (index, (width, height)) in [(64, 64), (65, 64), (64, 65)].into_iter().enumerate() {
        let path = source.join(format!("new-{index}.png"));
        write_png(&path, &photo(width, height));
        paths.push(path);
    }
    // The same bytes as the kept item: a duplicate, which adds no row.
    let twin = source.join("twin.png");
    std::fs::copy(&existing, &twin).unwrap();
    paths.push(twin);

    let report = import_paths(&library, &paths, &ImportOptions::default(), &mut |_| {});
    assert_eq!(report.imported, 3, "{report:?}");
    assert_eq!(report.duplicates, 1, "{report:?}");
    assert_eq!(report.created.len(), 3, "{report:?}");
    assert!(!report.created.contains(&kept.id), "{report:?}");

    let created_files: Vec<_> = report
        .created
        .iter()
        .map(|id| library.item(id).unwrap().unwrap().rel_path)
        .map(|rel| library.paths().abs(&rel))
        .collect();
    assert_eq!(library.delete_forever(&report.created).unwrap(), 3);

    let remaining = library.items(&TimelineFilter::default()).unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "only the item that was already here remains"
    );
    assert_eq!(remaining[0].id, kept.id);
    assert!(library.paths().abs(&kept.rel_path).is_file());
    for file in created_files {
        assert!(!file.exists(), "a taken-back import leaves no stored copy");
    }
}

#[test]
fn permanent_deletion_removes_all_preview_variants() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("photo.png");
    write_png(&source, &photo(48, 32));
    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_all(&library, &[source]);
    let id = &report.created[0];
    let paths = [
        library.paths().thumb_path(id),
        library.paths().viewer_jpeg_path(id),
        library.paths().viewer_png_path(id),
    ];
    for path in &paths {
        std::fs::write(path, b"cached preview").unwrap();
    }
    library.delete_forever(&report.created).unwrap();
    for path in &paths {
        assert!(
            !path.exists(),
            "preview survived deletion: {}",
            path.display()
        );
    }
}

#[test]
fn rejected_backup_preserves_an_existing_empty_destination() {
    let temp = TempDir::new().unwrap();
    let library = Library::open(temp.path().join("library")).unwrap();
    let destination = library.root().join("existing");
    std::fs::create_dir(&destination).unwrap();
    assert!(library.backup(&destination).is_err());
    assert!(destination.is_dir());
}

#[test]
fn emptying_the_trash_removes_every_row_and_file_in_one_pass() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let mut files = Vec::new();
    for index in 0..12u32 {
        let path = source.join(format!("frame-{index}.png"));
        write_png(&path, &photo(40 + index, 30));
        files.push(path);
    }
    let library = Library::open(temp.path().join("library")).unwrap();
    import_all(&library, &files);
    let ids: Vec<String> = library
        .items(&TimelineFilter::default())
        .unwrap()
        .into_iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(ids.len(), 12);
    library.trash(&ids).unwrap();

    assert_eq!(library.empty_trash().unwrap(), 12);
    assert!(
        library
            .items(&TimelineFilter::default())
            .unwrap()
            .is_empty()
    );
    let stored = walkdir::WalkDir::new(library.paths().store())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();
    assert_eq!(stored, 0, "files of deleted items were left behind");
}
