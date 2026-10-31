use archive_core::{Error, ImportOptions, Library, LockState, TimelineFilter, import_paths};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";

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

/// Import three distinct photos and return their ids in name order.
fn library_with_photos(root: &Path, sources: &Path) -> (Library, Vec<String>) {
    let mut paths = Vec::new();
    for (index, name) in ["a.jpg", "b.jpg", "c.jpg"].iter().enumerate() {
        let path: PathBuf = sources.join(name);
        write_jpeg(&path, 17 + index as u8 * 61);
        paths.push(path);
    }
    let library = Library::open(root).unwrap();
    import_paths(&library, &paths, &ImportOptions::default(), &mut |_| {});
    let mut items = library.items(&TimelineFilter::default()).unwrap();
    items.sort_by(|a, b| a.original_name.cmp(&b.original_name));
    let ids = items.into_iter().map(|item| item.id).collect();
    (library, ids)
}

fn visible_count(library: &Library) -> usize {
    library.item_count(&TimelineFilter::default()).unwrap()
}

fn collection(library: &Library, id: i64) -> archive_core::Collection {
    library
        .collections()
        .unwrap()
        .into_iter()
        .find(|collection| collection.id == id)
        .unwrap()
}

fn plaintext_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("store")];
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
fn locking_hides_items_and_unlocking_restores_them() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let original =
        std::fs::read(library.source_path(&library.item(&ids[0]).unwrap().unwrap())).unwrap();

    let other = library.create_collection("Other").unwrap();
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(other, &ids).unwrap();
    library.add_to_collection(private, &ids[..2]).unwrap();
    assert_eq!(visible_count(&library), 3);

    // Too short is refused before any key is derived.
    assert!(library.create_lock(private, "short").is_err());

    library.create_lock(private, PASSWORD).unwrap();
    assert_eq!(library.lock_state(private).unwrap(), LockState::Locked);
    // Hidden everywhere: the timeline, the collection, and the other collection.
    assert_eq!(visible_count(&library), 1);
    let private_filter = TimelineFilter {
        collection_id: Some(private),
        ..Default::default()
    };
    assert_eq!(library.item_count(&private_filter).unwrap(), 0);
    assert_eq!(collection(&library, other).count, 1);
    // Sealed items are not in the catalogue, so they belong to no visible collection.
    assert!(
        library
            .collection_ids_for_items(&ids[..2])
            .unwrap()
            .is_empty()
    );
    // The collection's own page shows the lock, not its cover or count.
    let locked = collection(&library, private);
    assert_eq!(locked.lock, LockState::Locked);
    assert_eq!(locked.count, 0);
    assert!(locked.cover_item_ids.is_empty());
    // No plaintext original is left in the store for a locked item.
    let store_files = plaintext_files(&root);
    assert_eq!(store_files.len(), 1, "only the unlocked photo is plaintext");
    assert!(
        !store_files
            .iter()
            .any(|file| std::fs::read(file).unwrap() == original)
    );
    // Locked items cannot be trashed or added to other collections.
    assert_eq!(library.trash(&ids[..1]).unwrap(), 0);
    assert_eq!(library.add_to_collection(other, &ids[..1]).unwrap(), 0);
    assert!(library.add_to_collection(private, &ids[2..]).is_err());

    // A wrong password changes nothing.
    assert!(matches!(
        library.unlock(private, "not the password"),
        Err(Error::WrongPassword)
    ));
    assert_eq!(library.lock_state(private).unwrap(), LockState::Locked);

    library.unlock(private, PASSWORD).unwrap();
    assert_eq!(library.lock_state(private).unwrap(), LockState::Unlocked);
    assert_eq!(library.item_count(&private_filter).unwrap(), 2);
    // Still hidden from the timeline while unlocked: only the unlocked-free photo shows.
    assert_eq!(visible_count(&library), 1);
    let restored = library.item(&ids[0]).unwrap().unwrap();
    assert_eq!(
        std::fs::read(library.source_path(&restored)).unwrap(),
        original
    );

    // Locking again seals the files; the store is plaintext-free for them.
    library.lock(private).unwrap();
    assert_eq!(library.lock_state(private).unwrap(), LockState::Locked);
    assert_eq!(library.item_count(&private_filter).unwrap(), 0);
    assert_eq!(plaintext_files(&root).len(), 1);
}

#[test]
fn a_crash_while_unlocked_stays_locked_until_the_password_is_entered() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(private, &ids).unwrap();
    library.create_lock(private, PASSWORD).unwrap();
    library.unlock(private, PASSWORD).unwrap();
    // Drop without locking, as a crash would. The key is gone with the process.
    drop(library);

    let library = Library::open(&root).unwrap();
    assert_eq!(library.lock_state(private).unwrap(), LockState::Locked);
    assert_eq!(visible_count(&library), 0);
    let filter = TimelineFilter {
        collection_id: Some(private),
        ..Default::default()
    };
    assert_eq!(library.item_count(&filter).unwrap(), 0);

    library.unlock(private, PASSWORD).unwrap();
    assert_eq!(library.item_count(&filter).unwrap(), 3);
}

#[test]
fn a_tampered_sealed_file_fails_to_unlock_and_leaves_nothing_behind() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(private, &ids[..1]).unwrap();
    library.create_lock(private, PASSWORD).unwrap();

    let sealed: Vec<PathBuf> = walk(&root.join("vault"))
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "blob"))
        .collect();
    assert_eq!(sealed.len(), 1);
    let mut bytes = std::fs::read(&sealed[0]).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x40;
    std::fs::write(&sealed[0], &bytes).unwrap();

    assert!(library.unlock(private, PASSWORD).is_err());
    assert_eq!(library.lock_state(private).unwrap(), LockState::Locked);
    // Only the two photos that were never locked remain as plaintext.
    assert_eq!(
        plaintext_files(&root).len(),
        2,
        "a failed unlock left plaintext behind"
    );
    assert_eq!(visible_count(&library), 2);
}

#[test]
fn removing_a_lock_returns_items_to_the_library() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(private, &ids[..2]).unwrap();
    library.create_lock(private, PASSWORD).unwrap();

    // A locked collection cannot be deleted, since that would lose its items.
    assert!(library.delete_collection(private).is_err());
    assert!(
        library.remove_lock(private).is_err(),
        "remove needs an unlock"
    );

    library.unlock(private, PASSWORD).unwrap();
    library.remove_lock(private).unwrap();
    assert_eq!(collection(&library, private).lock, LockState::None);
    assert_eq!(visible_count(&library), 3);
    assert_eq!(
        library
            .item_count(&TimelineFilter {
                collection_id: Some(private),
                ..Default::default()
            })
            .unwrap(),
        2
    );
    assert!(!root.join("vault").join(private.to_string()).exists());
}

#[test]
fn deleting_an_unlocked_collection_keeps_its_items() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(private, &ids[..2]).unwrap();
    library.create_lock(private, PASSWORD).unwrap();
    library.unlock(private, PASSWORD).unwrap();

    library.delete_collection(private).unwrap();
    assert!(library.collections().unwrap().is_empty());
    assert_eq!(visible_count(&library), 3);
}

#[test]
fn locking_all_seals_every_unlocked_collection_on_close() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let first = library.create_collection("First").unwrap();
    let second = library.create_collection("Second").unwrap();
    library.add_to_collection(first, &ids[..1]).unwrap();
    library.add_to_collection(second, &ids[1..2]).unwrap();
    library.create_lock(first, PASSWORD).unwrap();
    library.create_lock(second, PASSWORD).unwrap();
    library.unlock(first, PASSWORD).unwrap();
    library.unlock(second, PASSWORD).unwrap();
    assert_eq!(visible_count(&library), 1);

    assert_eq!(library.lock_all().unwrap(), 2);
    assert_eq!(library.lock_state(first).unwrap(), LockState::Locked);
    assert_eq!(library.lock_state(second).unwrap(), LockState::Locked);
    assert_eq!(visible_count(&library), 1);
    assert!(
        plaintext_files(&root).len() == 1,
        "only the ordinary photo is plaintext"
    );
}

#[test]
fn backups_carry_the_sealed_collections() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(private, &ids[..1]).unwrap();
    library.create_lock(private, PASSWORD).unwrap();

    let backup_root = work.path().join("backups");
    let report = library.backup(&backup_root).unwrap();
    let backed_up = Path::new(&report.path).join("vault");
    assert!(backed_up.is_dir());
    assert!(!walk(&backed_up).is_empty());
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).into_iter().flatten().flatten() {
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
fn unlock_restores_ready_previews_without_decoding_originals() {
    for transparent in [false, true] {
        let work = TempDir::new().unwrap();
        let root = work.path().join("lib");
        let source = work.path().join(if transparent {
            "photo.png"
        } else {
            "photo.jpg"
        });
        if transparent {
            image::RgbaImage::from_fn(96, 64, |x, y| image::Rgba([x as u8, y as u8, 100, 128]))
                .save(&source)
                .unwrap();
        } else {
            write_jpeg(&source, 17);
        }
        let library = Library::open(&root).unwrap();
        import_paths(&library, &[source], &ImportOptions::default(), &mut |_| {});
        let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
        let thumb = library.load_thumb_rgba(&item).unwrap();
        let viewer = library.load_preview_rgba(&item, 2048).unwrap();
        let paths = archive_core::Paths::preview_rel_paths(&item.id);
        let cached: Vec<_> = paths
            .iter()
            .filter_map(|relative| {
                std::fs::read(root.join(relative))
                    .ok()
                    .map(|bytes| (relative, bytes))
            })
            .collect();
        assert_eq!(cached.len(), 2);
        let private = library.create_collection("Private").unwrap();
        library
            .add_to_collection(private, std::slice::from_ref(&item.id))
            .unwrap();
        library.create_lock(private, PASSWORD).unwrap();
        for relative in &paths {
            assert!(
                !root.join(relative).exists(),
                "locked cache must not be plaintext"
            );
        }
        let sealed = walk(&root.join("vault"));
        assert_eq!(
            sealed
                .iter()
                .filter(|path| path.extension().is_some_and(|ext| ext == "preview"))
                .count(),
            2
        );
        for (_, bytes) in &cached {
            assert!(
                !sealed
                    .iter()
                    .any(|path| std::fs::read(path).unwrap() == *bytes)
            );
        }
        // Also exercise restoration after reopening the application.
        drop(library);
        let library = Library::open(&root).unwrap();
        for _ in 0..2 {
            library.unlock(private, PASSWORD).unwrap();
            let restored = library.item(&item.id).unwrap().unwrap();
            assert_eq!(restored.thumb_state, archive_core::ThumbState::Ready);
            for (relative, bytes) in &cached {
                assert_eq!(std::fs::read(root.join(relative)).unwrap(), *bytes);
            }
            // A missing original makes this fail if either loader tries to
            // regenerate its cache. Both must work immediately after unlock.
            let original = library.source_path(&restored);
            let bytes = std::fs::read(&original).unwrap();
            std::fs::remove_file(&original).unwrap();
            assert_eq!(library.load_thumb_rgba(&restored).unwrap(), thumb);
            assert_eq!(library.load_preview_rgba(&restored, 2048).unwrap(), viewer);
            std::fs::write(original, bytes).unwrap();
            library.lock(private).unwrap();
        }
    }
}

#[test]
fn tampered_preview_cleans_up_all_restored_plaintext() {
    let work = TempDir::new().unwrap();
    let sources = work.path().join("src");
    std::fs::create_dir_all(&sources).unwrap();
    let root = work.path().join("lib");
    let (library, ids) = library_with_photos(&root, &sources);
    let private = library.create_collection("Private").unwrap();
    library.add_to_collection(private, &ids).unwrap();
    library.create_lock(private, PASSWORD).unwrap();
    let previews: Vec<_> = walk(&root.join("vault"))
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "preview"))
        .collect();
    assert!(!previews.is_empty());
    // Every preview fails authentication, regardless of restore order.
    for path in previews {
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x40;
        std::fs::write(path, bytes).unwrap();
    }
    assert!(library.unlock(private, PASSWORD).is_err());
    assert_eq!(library.lock_state(private).unwrap(), LockState::Locked);
    assert!(plaintext_files(&root).is_empty());
    assert!(walk(&root.join("thumbs")).is_empty());
    assert!(walk(&root.join("tmp")).is_empty());
}
