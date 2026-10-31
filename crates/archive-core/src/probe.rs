use crate::audio;
use crate::error::{Error, Result};
use crate::ffmpeg;
use crate::model::Kind;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use exif::{In, Tag};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Everything the catalogue stores about a file's media properties.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub kind: Kind,
    pub mime: String,
    pub ext: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<i64>,
    pub captured_at: i64,
    pub captured_source: String,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub f_number: Option<f64>,
    pub exposure: Option<f64>,
    pub focal_mm: Option<f64>,
    pub orientation: i32,
    pub gps: Option<(f64, f64)>,
    /// Stream and tag details, for music files only.
    pub audio: Option<crate::audio::Meta>,
}

/// Read the file's signature and metadata. Unknown file types come back as
/// `Kind::Other`; the importer decides whether to keep them.
pub fn inspect(path: &Path, tools: &ffmpeg::Tools) -> Result<MediaInfo> {
    let mut file =
        File::open(path).map_err(|e| Error::Msg(format!("open {}: {e}", path.display())))?;
    let mut head = [0u8; 32];
    let read = file.read(&mut head).unwrap_or(0);
    let ext_hint = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let (kind, mime, ext) = detect(&head[..read], &ext_hint);

    let mut info = MediaInfo {
        kind,
        mime,
        ext,
        width: None,
        height: None,
        duration_ms: None,
        captured_at: 0,
        captured_source: "file time".into(),
        camera_make: None,
        camera_model: None,
        lens: None,
        iso: None,
        f_number: None,
        exposure: None,
        focal_mm: None,
        orientation: 1,
        gps: None,
        audio: None,
    };
    if kind == Kind::Other {
        info.captured_at = file_time_ms(path).unwrap_or_else(crate::time::now_ms);
        return Ok(info);
    }

    // Music is read entirely by ffprobe: the signature only says it is audio.
    // An `.ogg` can carry a video track, which makes it a video after all.
    let mut probed = false;
    if kind == Kind::Audio {
        let json = ffmpeg::ffprobe_json(tools, path)
            .ok_or_else(|| Error::Msg(format!("ffprobe could not read {}", path.display())))?;
        if audio::has_video(&json) {
            info.kind = Kind::Video;
            info.mime = "video/ogg".into();
        } else {
            let meta = audio::from_json(&json)
                .ok_or_else(|| Error::Msg(format!("{} has no audio stream", path.display())))?;
            info.duration_ms = meta.duration_ms;
            info.audio = Some(meta);
        }
        apply_ffprobe(&mut info, &json);
        if info.kind == Kind::Audio {
            // The cover's pixels are not the music's shape; a tile should not
            // be stretched to fit them.
            info.width = None;
            info.height = None;
        }
        probed = true;
    }

    if kind == Kind::Image && info.ext == "jxl" {
        // The image crate cannot read JPEG XL, so the size comes from the
        // codestream header. It is the size the decoder returns, rotation
        // included, which is the size every preview is drawn at.
        if let Ok(header) = crate::jxl::header(path) {
            info.width = Some(header.width);
            info.height = Some(header.height);
        }
    } else if kind == Kind::Image {
        // Dimensions and EXIF come from the one handle already open: a photo
        // import opens each source once, and the header and the tag block are
        // read from that same handle rather than reopening the file.
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(0)).is_ok() {
            if let Ok(probe) = image::ImageReader::new(&mut reader).with_guessed_format()
                && let Ok((width, height)) = probe.into_dimensions()
            {
                info.width = Some(width);
                info.height = Some(height);
            }
            if reader.seek(SeekFrom::Start(0)).is_ok()
                && let Ok(exif) = exif::Reader::new().read_from_container(&mut reader)
            {
                apply_exif(&mut info, &exif);
            }
        }
    }

    // ffprobe fills in anything the header decoders could not: video
    // streams, RAW/HEIF dimensions, odd containers. One probe is enough —
    // the JSON already carries the duration it would be asked for again.
    if !probed
        && (info.width.is_none() || info.height.is_none() || kind == Kind::Video)
        && let Some(json) = ffmpeg::ffprobe_json(tools, path)
    {
        apply_ffprobe(&mut info, &json);
    }
    if info.ext.is_empty() {
        info.ext = if ext_hint.is_empty() {
            "bin".into()
        } else {
            ext_hint
        };
    }

    if info.captured_at == 0 {
        if let Some((at, source)) = timestamp_from_filename(path) {
            info.captured_at = at;
            info.captured_source = source;
        } else if let Some(at) = file_time_ms(path) {
            info.captured_at = at;
            info.captured_source = "file time".into();
        } else {
            info.captured_at = crate::time::now_ms();
            info.captured_source = "import time".into();
        }
    }
    Ok(info)
}

fn apply_ffprobe(info: &mut MediaInfo, json: &serde_json::Value) {
    // A phone HEIF/AVIF is a grid of small tiles stitched into one picture.
    // Each tile is listed as its own video stream, so the first stream's size
    // is one tile; the picture's real size is on the tile-grid group.
    if let Some((width, height)) = tile_grid_size(json) {
        info.width = info.width.or(Some(width));
        info.height = info.height.or(Some(height));
    }
    let streams = json.get("streams").and_then(|value| value.as_array());
    if let Some(streams) = streams
        && let Some(video) = streams.iter().find(|stream| {
            stream.get("codec_type").and_then(|value| value.as_str()) == Some("video")
        })
    {
        let width = video.get("width").and_then(|value| value.as_u64());
        let height = video.get("height").and_then(|value| value.as_u64());
        let rotation = video
            .get("side_data_list")
            .and_then(|value| value.as_array())
            .and_then(|list| {
                list.iter().find_map(|entry| {
                    entry
                        .get("rotation")
                        .and_then(|value| value.as_i64())
                        .or_else(|| entry.get("rotate").and_then(|value| value.as_i64()))
                })
            })
            .or_else(|| {
                video
                    .get("tags")
                    .and_then(|tags| tags.get("rotate"))
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.parse::<i64>().ok())
            })
            .unwrap_or(0);
        let sideways = rotation.rem_euclid(180) == 90;
        if let (Some(width), Some(height)) = (width, height) {
            let (width, height) = if sideways {
                (height, width)
            } else {
                (width, height)
            };
            info.width = info.width.or(Some(width as u32));
            info.height = info.height.or(Some(height as u32));
        }
        if info.duration_ms.is_none() {
            info.duration_ms = video
                .get("duration")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<f64>().ok())
                .map(|seconds| (seconds * 1000.0).round() as i64);
        }
        if info.orientation == 1 && sideways {
            info.orientation = if rotation.rem_euclid(360) == 90 { 6 } else { 8 };
        }
    }
    if info.duration_ms.is_none() {
        info.duration_ms = json
            .get("format")
            .and_then(|format| format.get("duration"))
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<f64>().ok())
            .map(|seconds| (seconds * 1000.0).round() as i64);
    }
    if info.captured_at == 0 {
        let tags = json.get("format").and_then(|format| format.get("tags"));
        let date = tags.and_then(|tags| {
            tags.get("creation_time")
                .or_else(|| tags.get("date"))
                .and_then(|value| value.as_str())
        });
        if let Some(date) = date
            && let Some(ms) = parse_iso_datetime(date)
        {
            info.captured_at = ms;
            info.captured_source = "metadata".into();
        }
    }
}

fn detect(head: &[u8], ext_hint: &str) -> (Kind, String, String) {
    let starts = |sig: &[u8]| head.starts_with(sig);
    if starts(&[0xff, 0xd8, 0xff]) {
        return (Kind::Image, "image/jpeg".into(), "jpg".into());
    }
    if starts(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return (Kind::Image, "image/png".into(), "png".into());
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return (Kind::Image, "image/gif".into(), "gif".into());
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" {
        match &head[8..12] {
            b"WEBP" => return (Kind::Image, "image/webp".into(), "webp".into()),
            b"AVI " => return (Kind::Video, "video/x-msvideo".into(), "avi".into()),
            b"WAVE" => return (Kind::Audio, "audio/wav".into(), "wav".into()),
            _ => {}
        }
    }
    if starts(b"BM") {
        return (Kind::Image, "image/bmp".into(), "bmp".into());
    }
    if starts(&[0x49, 0x49, 0x2a, 0x00]) || starts(&[0x4d, 0x4d, 0x00, 0x2a]) {
        return (Kind::Image, "image/tiff".into(), "tiff".into());
    }
    if crate::jxl::is_jxl(head) {
        return (Kind::Image, "image/jxl".into(), "jxl".into());
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brand = String::from_utf8_lossy(&head[8..12]).to_ascii_lowercase();
        match brand.as_str() {
            "heic" | "heix" | "hevc" | "hevx" | "heim" | "heis" | "hevm" | "hevs" | "mif1"
            | "msf1" => return (Kind::Image, "image/heic".into(), "heic".into()),
            "avif" | "avis" => return (Kind::Image, "image/avif".into(), "avif".into()),
            "qt  " => return (Kind::Video, "video/quicktime".into(), "mov".into()),
            "m4v " | "m4vh" => return (Kind::Video, "video/x-m4v".into(), "m4v".into()),
            "m4a " | "m4b " => return (Kind::Audio, "audio/mp4".into(), "m4a".into()),
            _ => return (Kind::Video, "video/mp4".into(), "mp4".into()),
        }
    }
    if starts(&[0x1a, 0x45, 0xdf, 0xa3]) {
        let ext = if ext_hint == "webm" { "webm" } else { "mkv" };
        let mime = if ext == "webm" {
            "video/webm"
        } else {
            "video/x-matroska"
        };
        return (Kind::Video, mime.into(), ext.into());
    }
    if starts(&[0x30, 0x26, 0xb2, 0x75]) {
        // ASF holds both WMA audio and WMV video; the extension tells them apart.
        if ext_hint == "wma" {
            return (Kind::Audio, "audio/x-ms-wma".into(), "wma".into());
        }
        return (Kind::Video, "video/x-ms-wmv".into(), "wmv".into());
    }
    if starts(b"ID3") {
        return (Kind::Audio, "audio/mpeg".into(), "mp3".into());
    }
    if starts(b"fLaC") {
        return (Kind::Audio, "audio/flac".into(), "flac".into());
    }
    if starts(b"MAC ") {
        return (Kind::Audio, "audio/ape".into(), "ape".into());
    }
    if starts(b"wvpk") {
        return (Kind::Audio, "audio/wavpack".into(), "wv".into());
    }
    if head.len() >= 12 && &head[0..4] == b"FORM" && matches!(&head[8..12], b"AIFF" | b"AIFC") {
        return (Kind::Audio, "audio/aiff".into(), "aiff".into());
    }
    // An ADTS stream (raw AAC) starts with a sync word whose layer bits are
    // zero. Checked before MPEG audio, whose sync word it also matches.
    if head.len() >= 2 && head[0] == 0xff && head[1] & 0xf6 == 0xf0 {
        return (Kind::Audio, "audio/aac".into(), "aac".into());
    }
    if head.len() >= 2 && head[0] == 0xff && head[1] & 0xe0 == 0xe0 {
        return (Kind::Audio, "audio/mpeg".into(), "mp3".into());
    }
    if starts(b"FLV") {
        return (Kind::Video, "video/x-flv".into(), "flv".into());
    }
    if starts(&[0x00, 0x00, 0x01, 0xba]) || starts(&[0x00, 0x00, 0x01, 0xb3]) {
        return (Kind::Video, "video/mpeg".into(), "mpg".into());
    }
    if starts(b"OggS") {
        // Ogg is audio or video. An `.ogg` is taken as audio here, and
        // ffprobe promotes it to video if it finds a video track.
        match ext_hint {
            "opus" => return (Kind::Audio, "audio/opus".into(), "opus".into()),
            "ogg" | "oga" => return (Kind::Audio, "audio/ogg".into(), ext_hint.into()),
            _ => return (Kind::Other, "application/ogg".into(), "ogv".into()),
        }
    }

    // Extension fallback for containers without a reliable magic number
    // (transport streams, some RAW formats, image formats not sniffed above).
    let ext = normalized_ext(ext_hint);
    if is_raw_ext(&ext) {
        return (Kind::Image, "image/x-raw".into(), ext);
    }
    match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => (Kind::Image, "image/jpeg".into(), "jpg".into()),
        "png" => (Kind::Image, "image/png".into(), "png".into()),
        "gif" => (Kind::Image, "image/gif".into(), "gif".into()),
        "webp" => (Kind::Image, "image/webp".into(), "webp".into()),
        "bmp" => (Kind::Image, "image/bmp".into(), "bmp".into()),
        "tif" | "tiff" => (Kind::Image, "image/tiff".into(), "tiff".into()),
        "heic" | "heif" => (Kind::Image, "image/heic".into(), "heic".into()),
        "avif" => (Kind::Image, "image/avif".into(), "avif".into()),
        "jxl" => (Kind::Image, "image/jxl".into(), "jxl".into()),
        "ico" => (Kind::Image, "image/x-icon".into(), "ico".into()),
        "mp4" | "m4v" => (Kind::Video, "video/mp4".into(), ext),
        "mov" | "qt" => (Kind::Video, "video/quicktime".into(), "mov".into()),
        "webm" => (Kind::Video, "video/webm".into(), "webm".into()),
        "mkv" => (Kind::Video, "video/x-matroska".into(), "mkv".into()),
        "avi" => (Kind::Video, "video/x-msvideo".into(), "avi".into()),
        "wmv" => (Kind::Video, "video/x-ms-wmv".into(), "wmv".into()),
        "mpg" | "mpeg" | "m2v" => (Kind::Video, "video/mpeg".into(), "mpg".into()),
        "m2ts" | "mts" | "ts" => (Kind::Video, "video/mp2t".into(), "m2ts".into()),
        "3gp" | "3g2" => (Kind::Video, "video/3gpp".into(), "3gp".into()),
        "flv" => (Kind::Video, "video/x-flv".into(), "flv".into()),
        "ogv" => (Kind::Video, "video/ogg".into(), "ogv".into()),
        "mp3" => (Kind::Audio, "audio/mpeg".into(), ext),
        "m4a" | "m4b" => (Kind::Audio, "audio/mp4".into(), "m4a".into()),
        "aac" => (Kind::Audio, "audio/aac".into(), ext),
        "flac" => (Kind::Audio, "audio/flac".into(), ext),
        "wav" => (Kind::Audio, "audio/wav".into(), ext),
        "aif" | "aiff" | "aifc" => (Kind::Audio, "audio/aiff".into(), "aiff".into()),
        "ogg" | "oga" => (Kind::Audio, "audio/ogg".into(), ext),
        "opus" => (Kind::Audio, "audio/opus".into(), ext),
        "wma" => (Kind::Audio, "audio/x-ms-wma".into(), ext),
        "ape" => (Kind::Audio, "audio/ape".into(), ext),
        "wv" => (Kind::Audio, "audio/wavpack".into(), ext),
        "mpc" => (Kind::Audio, "audio/musepack".into(), ext),
        "tta" => (Kind::Audio, "audio/tta".into(), ext),
        _ => (Kind::Other, mime_for_other(&ext), ext),
    }
}

fn normalized_ext(ext: &str) -> String {
    match ext {
        "jpeg" | "jpe" | "jfif" => "jpg".into(),
        "tif" => "tiff".into(),
        "heif" => "heic".into(),
        other => other.to_ascii_lowercase(),
    }
}

fn is_raw_ext(ext: &str) -> bool {
    matches!(
        ext,
        "cr2"
            | "cr3"
            | "nef"
            | "nrw"
            | "arw"
            | "srf"
            | "sr2"
            | "orf"
            | "rw2"
            | "raf"
            | "dng"
            | "pef"
            | "srw"
            | "raw"
            | "rwl"
            | "3fr"
            | "erf"
            | "kdc"
    )
}

fn mime_for_other(ext: &str) -> String {
    match ext {
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
    .into()
}

fn exif_text(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let text = field.display_value().to_string();
    let text = text
        .trim()
        .trim_matches('"')
        .trim_matches('\0')
        .trim()
        .to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn exif_uint(exif: &exif::Exif, tag: Tag) -> Option<u32> {
    exif.get_field(tag, In::PRIMARY)?.value.get_uint(0)
}

fn rational_at(value: &exif::Value, index: usize) -> Option<f64> {
    match value {
        exif::Value::Rational(values) => values.get(index).map(|r| r.to_f64()),
        exif::Value::SRational(values) => values.get(index).map(|r| r.to_f64()),
        exif::Value::Short(values) => values.get(index).map(|v| *v as f64),
        exif::Value::Long(values) => values.get(index).map(|v| *v as f64),
        _ => None,
    }
}

fn exif_float(exif: &exif::Exif, tag: Tag) -> Option<f64> {
    rational_at(&exif.get_field(tag, In::PRIMARY)?.value, 0)
}

fn apply_exif(info: &mut MediaInfo, exif: &exif::Exif) {
    info.camera_make = exif_text(exif, Tag::Make);
    info.camera_model = exif_text(exif, Tag::Model);
    info.lens = exif_text(exif, Tag::LensModel);
    info.iso = exif_uint(exif, Tag::PhotographicSensitivity).map(|v| v as i64);
    info.f_number = exif_float(exif, Tag::FNumber);
    info.exposure = exif_float(exif, Tag::ExposureTime);
    info.focal_mm = exif_float(exif, Tag::FocalLength);
    if let Some(orientation) = exif_uint(exif, Tag::Orientation) {
        info.orientation = orientation.clamp(1, 8) as i32;
    }
    if let (Some(lat), Some(lon)) = (
        gps_coord(exif, Tag::GPSLatitude),
        gps_coord(exif, Tag::GPSLongitude),
    ) {
        // Zero/zero is the "no fix" value many phones write.
        if lat != 0.0 || lon != 0.0 {
            info.gps = Some((lat, lon));
        }
    }
    let date = exif_text(exif, Tag::DateTimeOriginal)
        .or_else(|| exif_text(exif, Tag::DateTimeDigitized))
        .or_else(|| exif_text(exif, Tag::DateTime));
    if let Some(date) = date {
        let offset =
            exif_text(exif, Tag::OffsetTimeOriginal).or_else(|| exif_text(exif, Tag::OffsetTime));
        if let Some(ms) = parse_exif_datetime(&date, offset.as_deref()) {
            info.captured_at = ms;
            info.captured_source = "exif".into();
        }
    }
}

fn gps_coord(exif: &exif::Exif, tag: Tag) -> Option<f64> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let value = &field.value;
    let rational = |index: usize| -> f64 { rational_at(value, index).unwrap_or(0.0) };
    let mut coord = rational(0) + rational(1) / 60.0 + rational(2) / 3600.0;
    let reference_tag = if tag == Tag::GPSLatitude {
        Tag::GPSLatitudeRef
    } else {
        Tag::GPSLongitudeRef
    };
    if let Some(reference) = exif_text(exif, reference_tag) {
        let first = reference.chars().next().unwrap_or('N').to_ascii_uppercase();
        if first == 'S' || first == 'W' {
            coord = -coord;
        }
    }
    Some(coord)
}

fn parse_exif_datetime(text: &str, offset: Option<&str>) -> Option<i64> {
    let text = text.trim().trim_matches('"').replace('\0', "");
    if let Some(offset) = offset {
        let joined = format!("{text} {offset}");
        for format in ["%Y:%m:%d %H:%M:%S %:z", "%Y-%m-%d %H:%M:%S %:z"] {
            if let Ok(datetime) = DateTime::parse_from_str(&joined, format) {
                return Some(datetime.timestamp_millis());
            }
        }
    }
    for format in [
        "%Y:%m:%d %H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y:%m:%d %H:%M:%S%.f",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&text, format) {
            return Some(local_to_ms(naive));
        }
    }
    None
}

fn local_to_ms(naive: NaiveDateTime) -> i64 {
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.timestamp_millis(),
        chrono::LocalResult::Ambiguous(early, _) => early.timestamp_millis(),
        chrono::LocalResult::None => Utc.from_utc_datetime(&naive).timestamp_millis(),
    }
}

fn parse_iso_datetime(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .or_else(|| {
            NaiveDateTime::parse_from_str(text.trim_end_matches('Z'), "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(local_to_ms)
        })
}

fn file_time_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

/// Look for a capture time in common camera filenames. A photo named
/// `IMG_20230102_123456` should sort with the day it was taken, not the
/// day it was copied.
fn timestamp_from_filename(path: &Path) -> Option<(i64, String)> {
    let name = path.file_name()?.to_str()?;
    let bytes = name.as_bytes();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(from) = start.take() {
            runs.push((from, index));
        }
    }
    if let Some(from) = start {
        runs.push((from, bytes.len()));
    }
    let digits = |run: (usize, usize)| -> u32 { name[run.0..run.1].parse::<u32>().unwrap_or(0) };
    let date_from = |value: u32| -> Option<NaiveDate> {
        if !(1900..=2200).contains(&(value / 10_000)) {
            return None;
        }
        NaiveDate::from_ymd_opt((value / 10_000) as i32, value / 100 % 100, value % 100)
    };
    for (index, run) in runs.iter().enumerate() {
        let length = run.1 - run.0;
        // Compact: YYYYMMDD [ _-] HHMMSS, or one 14-digit run.
        if length == 8
            && let Some(date) = date_from(digits(*run))
            && let Some(time) = runs.get(index + 1).filter(|next| next.1 - next.0 == 6)
        {
            let value = digits(*time);
            if let Some(naive) = combine(date, value / 10_000, value / 100 % 100, value % 100) {
                return Some((local_to_ms(naive), "filename".into()));
            }
        }
        if length == 14 {
            let value = digits(*run);
            if let Some(date) = date_from(value / 1_000_000) {
                let time = value % 1_000_000;
                if let Some(naive) = combine(date, time / 10_000, time / 100 % 100, time % 100) {
                    return Some((local_to_ms(naive), "filename".into()));
                }
            }
        }
        // Separated: 2023-01-02 12.34.56 or 2023 01 02 12 34 56.
        if length == 4 && (1900..=2200).contains(&digits(*run)) && runs.len() >= index + 6 {
            let year = digits(*run) as i32;
            let part = |offset: usize| -> Option<u32> {
                let next = runs.get(index + offset)?;
                if next.1 - next.0 > 2 {
                    return None;
                }
                Some(digits(*next))
            };
            if let (Some(month), Some(day), Some(hour), Some(minute), Some(second)) =
                (part(1), part(2), part(3), part(4), part(5))
                && let Some(date) = NaiveDate::from_ymd_opt(year, month, day)
                && let Some(naive) = combine(date, hour, minute, second)
            {
                return Some((local_to_ms(naive), "filename".into()));
            }
        }
    }
    None
}

fn combine(date: NaiveDate, hour: u32, minute: u32, second: u32) -> Option<NaiveDateTime> {
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    date.and_hms_opt(hour, minute, second)
}

/// The output size of the first tile-grid stream group, turned for a
/// sideways display rotation.
fn tile_grid_size(json: &serde_json::Value) -> Option<(u32, u32)> {
    let groups = json.get("stream_groups")?.as_array()?;
    let grid = groups
        .iter()
        .find(|group| group.get("type").and_then(|value| value.as_str()) == Some("Tile Grid"))?;
    let component = grid.get("components")?.as_array()?.first()?;
    let width = u32::try_from(component.get("width")?.as_u64()?).ok()?;
    let height = u32::try_from(component.get("height")?.as_u64()?).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let rotation = component
        .get("side_data_list")
        .and_then(|value| value.as_array())
        .and_then(|list| {
            list.iter()
                .find_map(|entry| entry.get("rotation").and_then(|value| value.as_i64()))
        })
        .unwrap_or(0);
    Some(if rotation.rem_euclid(180) == 90 {
        (height, width)
    } else {
        (width, height)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_tile_grid_reports_the_stitched_size_not_a_tile() {
        let json = serde_json::json!({
            "streams": [{ "codec_type": "video", "width": 512, "height": 512 }],
            "stream_groups": [{
                "type": "Tile Grid",
                "components": [{ "width": 4000, "height": 3000, "side_data_list": [
                    { "side_data_type": "EXIF metadata" }
                ] }]
            }]
        });
        assert_eq!(tile_grid_size(&json), Some((4000, 3000)));
        let mut turned = json.clone();
        turned["stream_groups"][0]["components"][0]["side_data_list"] =
            serde_json::json!([{ "side_data_type": "Display Matrix", "rotation": -90 }]);
        assert_eq!(tile_grid_size(&turned), Some((3000, 4000)));
        assert_eq!(tile_grid_size(&serde_json::json!({ "streams": [] })), None);
    }

    #[test]
    fn signature_beats_wrong_extension() {
        let head = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
        let (kind, mime, ext) = detect(&head, "jpg");
        assert_eq!(kind, Kind::Image);
        assert_eq!(mime, "image/png");
        assert_eq!(ext, "png");
    }

    #[test]
    fn heif_and_avif_are_images() {
        let mut head = [0u8; 32];
        head[4..8].copy_from_slice(b"ftyp");
        head[8..12].copy_from_slice(b"heic");
        assert_eq!(detect(&head, "bin").0, Kind::Image);
        head[8..12].copy_from_slice(b"avif");
        let (kind, _, ext) = detect(&head, "bin");
        assert_eq!(kind, Kind::Image);
        assert_eq!(ext, "avif");
    }

    #[test]
    fn filename_timestamp_is_read() {
        let path = PathBuf::from("IMG_20230102_123456.jpg");
        let (ms, source) = timestamp_from_filename(&path).expect("timestamp");
        assert_eq!(source, "filename");
        let year = 2023;
        assert_eq!(
            chrono::Local
                .timestamp_millis_opt(ms)
                .unwrap()
                .format("%Y")
                .to_string(),
            year.to_string()
        );
    }

    #[test]
    fn separated_filename_timestamp_is_read() {
        let path = PathBuf::from("Screenshot 2024-05-06 07.08.09.png");
        assert!(timestamp_from_filename(&path).is_some());
    }

    #[test]
    fn random_numbers_are_not_dates() {
        let path = PathBuf::from("build_1234567890.log");
        assert!(timestamp_from_filename(&path).is_none());
    }
}
