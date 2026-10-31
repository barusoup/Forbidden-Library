//! JPEG XL through the whole import flow: the optimizer's lossless rewrite,
//! and the previews and full decode that read the stored file back.

use archive_core::{
    ImportOptions, Library, OptState, OptimizeConfig, TimelineFilter, Tools, import_paths,
};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, Rgb, RgbImage};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use tempfile::TempDir;

/// Point the engine at the bundled JPEG XL tools for this process. The engine
/// caches its discovery, so this must run before any test here discovers
/// tools, and every test calls it first. Returns false when the tools are not
/// present, and the caller skips.
fn jxl_tools_present() -> bool {
    let dir = std::env::var_os("FORBIDDEN_LIBRARY_JXL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/jxl"));
    if !dir.join("cjxl.exe").is_file() || !dir.join("djxl.exe").is_file() {
        return false;
    }
    static POINT: Once = Once::new();
    POINT.call_once(|| {
        // SAFETY: set before this process starts any engine work, because every
        // test calls this first and `Once` makes the others wait for it.
        unsafe {
            std::env::set_var("FORBIDDEN_LIBRARY_JXL_DIR", &dir);
        }
    });
    true
}

/// A photograph-like picture: smooth gradients with a little noise, so it
/// compresses the way a real photo does rather than as a flat graphic.
fn photo(width: u32, height: u32) -> RgbImage {
    let mut image = RgbImage::new(width, height);
    let mut seed = 0x9e37_79b9u32;
    for y in 0..height {
        for x in 0..width {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (seed >> 24) as u8 / 16;
            let r = ((x * 255 / width.max(1)) as u8).saturating_add(noise);
            let g = ((y * 255 / height.max(1)) as u8).saturating_add(noise);
            let b = (((x + y) * 255 / (width + height).max(1)) as u8).saturating_add(noise);
            image.put_pixel(x, y, Rgb([r, g, b]));
        }
    }
    image
}

fn jpeg_bytes(image: &RgbImage, orientation: Option<u16>) -> Vec<u8> {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 92)
        .encode_image(&DynamicImage::ImageRgb8(image.clone()))
        .unwrap();
    if let Some(orientation) = orientation {
        // One IFD holding a single Orientation entry, in a big-endian TIFF.
        let mut tiff = b"MM\0*".to_vec();
        tiff.extend_from_slice(&8u32.to_be_bytes());
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x0112u16.to_be_bytes());
        tiff.extend_from_slice(&3u16.to_be_bytes());
        tiff.extend_from_slice(&1u32.to_be_bytes());
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_be_bytes());
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(&payload);
        jpeg.splice(2..2, segment);
    }
    jpeg
}

fn import_with(library: &Library, paths: &[PathBuf], optimize: bool) -> archive_core::ImportReport {
    let options = ImportOptions {
        optimize: optimize.then(OptimizeConfig::default),
        ..Default::default()
    };
    import_paths(library, paths, &options, &mut |_| {})
}

#[test]
fn a_jpeg_is_stored_as_jpeg_xl_and_rebuilds_byte_for_byte() {
    if !jxl_tools_present() {
        eprintln!("skipping: assets/jxl is not present");
        return;
    }
    let temp = TempDir::new().unwrap();
    let jpg = temp.path().join("harbour.jpg");
    let original = jpeg_bytes(&photo(640, 480), None);
    std::fs::write(&jpg, &original).unwrap();

    let library = Library::open(temp.path().join("library")).unwrap();
    let report = import_with(&library, &[jpg], true);
    assert_eq!(report.imported, 1, "{report:?}");
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);

    assert_eq!(item.opt_state, OptState::Optimized, "{:?}", item.opt_error);
    assert_eq!(item.ext, "jxl");
    assert_eq!(item.mime, "image/jxl");
    assert_eq!(item.opt_kind.as_deref(), Some("JPEG XL (reversible)"));
    assert!(
        item.bytes < item.original_bytes,
        "{} is not smaller than {}",
        item.bytes,
        item.original_bytes
    );
    // The identity is still the hash of the original JPEG's bytes.
    assert_eq!(item.id, blake3::hash(&original).to_hex().to_string());

    library.make_thumbnail(&item).unwrap();
    let (width, height, _) = library.load_full_rgba(&item, 0).unwrap();
    assert_eq!((width, height), (640, 480));
    assert!(library.load_preview_rgba(&item, 2048).is_ok());

    let stored = library.paths().abs(&item.rel_path);
    let rebuilt = temp.path().join("rebuilt.jpg");
    let djxl = Tools::discover().djxl.expect("djxl is present");
    let status = Command::new(djxl)
        .arg(&stored)
        .arg(&rebuilt)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&rebuilt).unwrap(), original);
    assert_eq!(
        blake3::hash(&std::fs::read(&rebuilt).unwrap())
            .to_hex()
            .to_string(),
        item.id
    );
}

#[test]
fn a_rotated_jpeg_is_shown_upright_once_after_optimization() {
    if !jxl_tools_present() {
        eprintln!("skipping: assets/jxl is not present");
        return;
    }
    let temp = TempDir::new().unwrap();
    // Stored as 640x480, displayed as 480x640 because of its EXIF tag.
    let jpg = temp.path().join("portrait.jpg");
    std::fs::write(&jpg, jpeg_bytes(&photo(640, 480), Some(6))).unwrap();

    let plain = Library::open(temp.path().join("plain")).unwrap();
    assert_eq!(
        import_with(&plain, std::slice::from_ref(&jpg), false).imported,
        1
    );
    let plain_item = plain.items(&TimelineFilter::default()).unwrap().remove(0);

    let optimized = Library::open(temp.path().join("optimized")).unwrap();
    assert_eq!(import_with(&optimized, &[jpg], true).imported, 1);
    let optimized_item = optimized
        .items(&TimelineFilter::default())
        .unwrap()
        .remove(0);
    assert_eq!(optimized_item.ext, "jxl");
    assert_eq!(optimized_item.orientation, 6);

    for (library, item) in [(&plain, &plain_item), (&optimized, &optimized_item)] {
        let (width, height, _) = library.load_full_rgba(item, 0).unwrap();
        assert_eq!((width, height), (480, 640), "full view of {}", item.ext);
        let (width, height, _) = library.load_preview_rgba(item, 2048).unwrap();
        assert_eq!((width, height), (480, 640), "preview of {}", item.ext);
    }
}

#[test]
fn an_optimized_png_shows_exactly_its_own_pixels() {
    if !jxl_tools_present() {
        eprintln!("skipping: assets/jxl is not present");
        return;
    }
    let temp = TempDir::new().unwrap();
    let png = temp.path().join("smooth.png");
    let source = photo(512, 384);
    source
        .save_with_format(&png, image::ImageFormat::Png)
        .unwrap();

    let library = Library::open(temp.path().join("library")).unwrap();
    assert_eq!(import_with(&library, &[png], true).imported, 1);
    let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
    assert!(
        matches!(item.ext.as_str(), "png" | "webp" | "jxl"),
        "unexpected {}",
        item.ext
    );

    // Lossless means the stored form decodes back to the source samples.
    let (width, height, rgba) = library.load_full_rgba(&item, 0).unwrap();
    assert_eq!((width, height), (512, 384));
    assert_eq!(rgba, DynamicImage::ImageRgb8(source).to_rgba8().into_raw());
}
