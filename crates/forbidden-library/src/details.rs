//! What the Details panel says about one item. The facts are grouped into
//! blocks, each with a summary that stays visible, so the panel reads as a
//! few clear sections instead of one long list. A block only lists what was
//! actually recorded for the file.

use crate::icons::AppIcon;
use archive_core::{AudioMeta, Item, OptState, ThumbState, format_datetime, human_bytes};

/// Number of blocks. Each keeps its own open or closed state.
pub const BLOCK_COUNT: usize = 4;
/// Height of one field cell, and the gap between rows of cells.
pub const FIELD_H: f32 = 48.0;
pub const FIELD_GAP: f32 = 10.0;

pub struct Field {
    pub label: &'static str,
    pub value: String,
    /// Takes the full width of the block body instead of half of it.
    pub wide: bool,
}

pub struct Block {
    pub icon: AppIcon,
    pub title: String,
    /// Empty when there is nothing to add under the title.
    pub summary: String,
    pub fields: Vec<Field>,
    /// False when the file has nothing for this block. Such a block is greyed
    /// out and cannot open.
    pub available: bool,
}

/// A field's place inside a block body, in body coordinates.
pub struct Placed {
    pub field: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
}

/// Lays fields out two to a row. Wide fields, and a field left alone at the
/// end, take a full row. Returns the placements and the body height.
pub fn place(fields: &[Field], width: f32) -> (Vec<Placed>, f32) {
    let half = (width - FIELD_GAP) * 0.5;
    let mut placed = Vec::with_capacity(fields.len());
    let mut y = 0.0;
    let mut index = 0;
    while index < fields.len() {
        let pair = !fields[index].wide && fields.get(index + 1).is_some_and(|next| !next.wide);
        if pair {
            placed.push(Placed {
                field: index,
                x: 0.0,
                y,
                w: half,
            });
            placed.push(Placed {
                field: index + 1,
                x: half + FIELD_GAP,
                y,
                w: half,
            });
            index += 2;
        } else {
            let w = if fields[index].wide { width } else { half };
            placed.push(Placed {
                field: index,
                x: 0.0,
                y,
                w,
            });
            index += 1;
        }
        y += FIELD_H + FIELD_GAP;
    }
    let height = if fields.is_empty() {
        0.0
    } else {
        y - FIELD_GAP
    };
    (placed, height)
}

/// The detail blocks for an item, in display order. Music puts its tags where
/// the camera would be, since a track has no camera or location tab.
pub fn blocks(item: &Item, audio: Option<&AudioMeta>) -> Vec<Block> {
    match audio {
        Some(audio) => vec![music(item, audio), date(item), file(item, Some(audio))],
        None => vec![camera(item), date(item), location(item), file(item, None)],
    }
}

fn field(label: &'static str, value: impl Into<String>, wide: bool) -> Field {
    Field {
        label,
        value: value.into(),
        wide,
    }
}

fn camera(item: &Item) -> Block {
    let name = camera_name(item);
    let aperture = item.f_number.filter(|f| *f > 0.0).map(aperture_text);
    let shutter = item.exposure.filter(|e| *e > 0.0).map(shutter_text);
    let iso = item.iso.map(|iso| iso.to_string());
    let focal = item
        .focal_mm
        .filter(|mm| *mm > 0.0)
        .map(|mm| format!("{} mm", mm.round() as i64));
    let lens = item.lens.clone().filter(|lens| !lens.trim().is_empty());

    let summary = [
        aperture.clone(),
        shutter.clone(),
        iso.as_ref().map(|iso| format!("ISO {iso}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let mut fields = Vec::new();
    if let Some(lens) = lens {
        fields.push(field("Lens", lens, true));
    }
    if let Some(focal) = focal {
        fields.push(field("Focal length", focal, false));
    }
    if let Some(aperture) = aperture {
        fields.push(field("Aperture", aperture, false));
    }
    if let Some(shutter) = shutter {
        fields.push(field("Shutter speed", shutter, false));
    }
    if let Some(iso) = iso {
        fields.push(field("ISO", iso, false));
    }

    let available = name.is_some() || !fields.is_empty();
    Block {
        icon: AppIcon::Aperture,
        title: name.unwrap_or_else(|| "Camera".to_string()),
        summary: if available { summary } else { String::new() },
        fields,
        available,
    }
}

/// "Canon EOS R5" from a make and model, without repeating the make when the
/// model already starts with it.
fn camera_name(item: &Item) -> Option<String> {
    let make = item
        .camera_make
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let model = item
        .camera_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (make, model) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model.to_string())
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (None, Some(model)) => Some(model.to_string()),
        (Some(make), None) => Some(make.to_string()),
        (None, None) => None,
    }
}

fn date(item: &Item) -> Block {
    // The formatted text is "7 March 2024 at 14:05"; the title takes the day
    // and the summary takes the time.
    let text = format_datetime(item.captured_at);
    let (day, time) = match text.split_once(" at ") {
        Some((day, time)) => (day.to_string(), time.to_string()),
        None => (text, String::new()),
    };
    let source = match item.captured_source.as_str() {
        "exif" => "From photo metadata",
        "filename" => "From the file name",
        _ => "From the file date",
    };
    Block {
        icon: AppIcon::Calendar,
        title: day,
        summary: time,
        fields: vec![
            field("Date source", source, true),
            field("Added to archive", format_datetime(item.added_at), true),
        ],
        available: true,
    }
}

fn location(item: &Item) -> Block {
    match item.gps {
        Some((lat, lon)) => Block {
            icon: AppIcon::MapPin,
            title: "Location".into(),
            summary: format!(
                "{:.4}° {}, {:.4}° {}",
                lat.abs(),
                if lat >= 0.0 { "N" } else { "S" },
                lon.abs(),
                if lon >= 0.0 { "E" } else { "W" },
            ),
            fields: vec![
                field("Latitude", format!("{lat:.6}°"), false),
                field("Longitude", format!("{lon:.6}°"), false),
            ],
            available: true,
        },
        None => Block {
            icon: AppIcon::MapPin,
            title: "Location".into(),
            summary: String::new(),
            fields: Vec::new(),
            available: false,
        },
    }
}

fn file(item: &Item, audio: Option<&AudioMeta>) -> Block {
    let size = human_bytes(item.bytes);
    let dimensions = item
        .display_size()
        .map(|(w, h)| (w, h, format!("{w} × {h}")));
    let summary = match &dimensions {
        Some((_, _, text)) => format!("{text} · {size}"),
        None => size.clone(),
    };

    let mut fields = Vec::new();
    if let Some((w, h, text)) = &dimensions {
        fields.push(field("Dimensions", text.clone(), false));
        let megapixels = f64::from(*w) * f64::from(*h) / 1_000_000.0;
        fields.push(field("Resolution", format!("{megapixels:.1} MP"), false));
    }
    fields.push(field("Size", size, false));
    if let Some(ms) = item.duration_ms {
        fields.push(field("Duration", duration_text(ms), false));
    }
    if let Some(audio) = audio {
        let mut quality = Vec::new();
        if let Some(bps) = audio.bitrate.filter(|bps| *bps > 0) {
            quality.push(format!("{} kbps", (bps + 500) / 1000));
        }
        if let Some(rate) = audio.sample_rate.filter(|rate| *rate > 0) {
            quality.push(format!("{:.1} kHz", rate as f64 / 1000.0));
        }
        if !quality.is_empty() {
            fields.push(field("Audio", quality.join(" · "), false));
        }
    }
    // A preview or a rewrite that failed is a fact about the file, not a
    // silent placeholder: say what the decoder or the verifier said.
    if item.thumb_state == ThumbState::Failed {
        let reason = item
            .thumb_error
            .as_deref()
            .unwrap_or("the decoder rejected the file");
        fields.push(field("Preview", format!("Failed — {reason}"), true));
    }
    if item.opt_state == OptState::Failed {
        let reason = item
            .opt_error
            .as_deref()
            .unwrap_or("the rewrite could not be verified");
        fields.push(field("Optimization", format!("Failed — {reason}"), true));
    }

    Block {
        icon: AppIcon::FileImage,
        title: format_label(&item.ext),
        summary,
        fields,
        available: true,
    }
}

/// How a file type reads on screen: "WebP", "JPEG", and so on. Unknown
/// extensions are shown in capitals.
fn format_label(ext: &str) -> String {
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "JPEG".into(),
        "webp" => "WebP".into(),
        "png" => "PNG".into(),
        "gif" => "GIF".into(),
        "heic" | "heif" => "HEIC".into(),
        "avif" => "AVIF".into(),
        "jxl" => "JPEG XL".into(),
        "tif" | "tiff" => "TIFF".into(),
        other => other.to_uppercase(),
    }
}

/// "1/250 s" for quick exposures, "2 s" or "1.5 s" for long ones.
pub fn shutter_text(seconds: f64) -> String {
    if seconds >= 0.5 {
        if (seconds - seconds.round()).abs() < 0.05 {
            format!("{} s", seconds.round() as i64)
        } else {
            format!("{seconds:.1} s")
        }
    } else {
        format!("1/{} s", (1.0 / seconds).round() as i64)
    }
}

/// "f/2.8", or "f/8" when the stop is whole.
pub fn aperture_text(f_number: f64) -> String {
    if (f_number - f_number.round()).abs() < 0.05 {
        format!("f/{}", f_number.round() as i64)
    } else {
        format!("f/{f_number:.1}")
    }
}

fn duration_text(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// What a music file says about itself: its tags and stream quality.
fn music(item: &Item, audio: &AudioMeta) -> Block {
    let tags = &audio.tags;
    let title = tags
        .title
        .clone()
        .unwrap_or_else(|| item.original_name.clone());
    let credit = [tags.artist.clone(), tags.album.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

    let mut fields = Vec::new();
    if let Some(artist) = &tags.artist {
        fields.push(field("Artist", artist.clone(), true));
    }
    if let Some(album) = &tags.album {
        fields.push(field("Album", album.clone(), true));
    }
    if let Some(album_artist) = &tags.album_artist
        && tags.artist.as_deref() != Some(album_artist.as_str())
    {
        fields.push(field("Album artist", album_artist.clone(), false));
    }
    match (tags.track_no, tags.track_total) {
        (Some(no), Some(total)) => fields.push(field("Track", format!("{no} of {total}"), false)),
        (Some(no), None) => fields.push(field("Track", no.to_string(), false)),
        _ => {}
    }
    if let Some(disc) = tags.disc_no {
        fields.push(field("Disc", disc.to_string(), false));
    }
    if let Some(year) = tags.year {
        fields.push(field("Year", year.to_string(), false));
    }
    if let Some(genre) = &tags.genre {
        fields.push(field("Genre", genre.clone(), false));
    }

    Block {
        icon: AppIcon::Music,
        title,
        summary: credit,
        fields,
        available: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_at(label: &'static str, wide: bool) -> Field {
        field(label, "x", wide)
    }

    #[test]
    fn short_fields_pair_up_and_wide_fields_take_a_row() {
        let fields = [
            field_at("a", false),
            field_at("b", false),
            field_at("c", true),
            field_at("d", false),
        ];
        let (placed, height) = place(&fields, 280.0);
        assert_eq!(placed.len(), 4);
        assert_eq!(placed[1].x, placed[0].w + FIELD_GAP);
        assert_eq!(placed[2].w, 280.0);
        // Three rows: the pair, the wide field, and the lone last field.
        assert_eq!(height, 3.0 * FIELD_H + 2.0 * FIELD_GAP);
    }

    #[test]
    fn empty_block_has_no_body() {
        assert_eq!(place(&[], 280.0).1, 0.0);
    }

    #[test]
    fn exposure_and_aperture_read_like_a_camera_label() {
        assert_eq!(shutter_text(1.0 / 250.0), "1/250 s");
        assert_eq!(shutter_text(2.0), "2 s");
        assert_eq!(shutter_text(1.5), "1.5 s");
        assert_eq!(aperture_text(2.8), "f/2.8");
        assert_eq!(aperture_text(8.0), "f/8");
    }

    fn item() -> Item {
        Item {
            id: "item".into(),
            kind: archive_core::Kind::Image,
            mime: "image/jpeg".into(),
            ext: "jpg".into(),
            original_ext: "jpg".into(),
            bytes: 2048,
            original_bytes: 2048,
            width: Some(1920),
            height: Some(1080),
            duration_ms: None,
            captured_at: 0,
            captured_source: "exif".into(),
            added_at: 0,
            modified_at: 0,
            original_name: "photo.jpg".into(),
            rel_path: "store/photo".into(),
            thumb_state: archive_core::ThumbState::Ready,
            opt_state: archive_core::OptState::None,
            opt_kind: None,
            deleted_at: None,
            camera_make: None,
            camera_model: None,
            lens: None,
            iso: None,
            f_number: None,
            exposure: None,
            focal_mm: None,
            orientation: 1,
            gps: None,
            avg_color: 0,
            checksum: "0123456789abcdef".into(),
            content_hash: "0123456789abcdef".into(),
            thumb_recipe: archive_core::THUMB_RECIPE,
            thumb_error: None,
            opt_error: None,
        }
    }

    #[test]
    fn file_details_never_show_the_checksum() {
        let block = file(&item(), None);
        assert!(
            block.fields.iter().all(|field| field.label != "Checksum"),
            "the checksum is internal and must not be shown"
        );
    }

    #[test]
    fn a_failed_preview_says_why() {
        let mut broken = item();
        broken.thumb_state = ThumbState::Failed;
        broken.thumb_error = Some("unsupported codec".into());
        let block = file(&broken, None);
        assert!(
            block
                .fields
                .iter()
                .any(|field| field.label == "Preview" && field.value.contains("unsupported codec")),
            "a failed preview must report its reason"
        );
    }
}
