use crate::audio::Meta as AudioMeta;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What a stored file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Image,
    Video,
    Audio,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "image" => Kind::Image,
            "video" => Kind::Video,
            "audio" => Kind::Audio,
            _ => Kind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThumbState {
    Pending,
    Ready,
    Failed,
    None,
}

impl ThumbState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThumbState::Pending => "pending",
            ThumbState::Ready => "ready",
            ThumbState::Failed => "failed",
            ThumbState::None => "none",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "pending" => ThumbState::Pending,
            "ready" => ThumbState::Ready,
            "failed" => ThumbState::Failed,
            _ => ThumbState::None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptState {
    Pending,
    None,
    Optimized,
    Failed,
    Skipped,
}

impl OptState {
    pub fn as_str(self) -> &'static str {
        match self {
            OptState::Pending => "pending",
            OptState::None => "none",
            OptState::Optimized => "optimized",
            OptState::Failed => "failed",
            OptState::Skipped => "skipped",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "pending" => OptState::Pending,
            "optimized" => OptState::Optimized,
            "failed" => OptState::Failed,
            "skipped" => OptState::Skipped,
            _ => OptState::None,
        }
    }
}

/// One archived file. `id` is the hash of the bytes as first imported and
/// never changes; `checksum` describes the file currently in the store.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub kind: Kind,
    pub mime: String,
    pub ext: String,
    pub original_ext: String,
    pub bytes: u64,
    pub original_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<i64>,
    /// Epoch ms; always present (falling back to file time).
    pub captured_at: i64,
    /// exif | filename | file-time
    pub captured_source: String,
    pub added_at: i64,
    pub modified_at: i64,
    pub original_name: String,
    pub rel_path: String,
    pub thumb_state: ThumbState,
    pub opt_state: OptState,
    pub opt_kind: Option<String>,
    pub deleted_at: Option<i64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub f_number: Option<f64>,
    pub exposure: Option<f64>,
    pub focal_mm: Option<f64>,
    pub orientation: i32,
    pub gps: Option<(f64, f64)>,
    /// 0xRRGGBB average colour, drawn behind the tile while it loads.
    pub avg_color: u32,
    pub checksum: String,
    pub content_hash: String,
    /// Which preview recipe produced the cached thumbnail. A preview built by
    /// an older recipe is rebuilt on next use instead of being shown stale.
    pub thumb_recipe: i64,
    /// Why the last preview attempt failed, for the details panel.
    pub thumb_error: Option<String>,
    /// Why the last optimization attempt failed, for the details panel.
    pub opt_error: Option<String>,
}

impl Item {
    /// The size the picture is shown at, with the EXIF quarter turn applied.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some(if self.turned_by_the_app() {
                (h, w)
            } else {
                (w, h)
            }),
            _ => None,
        }
    }

    pub fn aspect(&self) -> f32 {
        self.display_size()
            .map_or(1.0, |(w, h)| w as f32 / h as f32)
    }

    /// Whether the app itself turns the pixels of this item. Only the images
    /// its own decoders read are stored with their raw size and orientation:
    /// a video's probe has already swapped its size, and ffmpeg's stills are
    /// already rotated, so turning those again would be wrong. A JPEG XL
    /// rotated by its codestream still reports the original's orientation,
    /// and its raw size, so the same swap applies to it.
    fn turned_by_the_app(&self) -> bool {
        self.kind == Kind::Image
            && matches!(
                self.ext.as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "tif" | "tiff" | "bmp" | "gif" | "jxl"
            )
            && (5..=8).contains(&self.orientation)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TimelineFilter {
    pub trashed: bool,
    pub kind: Option<Kind>,
    pub collection_id: Option<i64>,
    pub limit: Option<usize>,
    /// Matches nothing. Set for a locked collection that is not unlocked, so
    /// its contents cannot be read even if rows for them are still present.
    pub none: bool,
}

/// How many images a collection's cover layers, one sheet on top of another.
pub const COVER_LAYERS: usize = 3;

/// Whether a collection is protected by a password, and if so whether its
/// contents are readable right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockState {
    /// An ordinary collection.
    #[default]
    None,
    /// Password-locked and sealed. Its items are not in the catalogue at all.
    Locked,
    /// Password-locked and unlocked for this session. Its items are readable,
    /// and hidden from every other page.
    Unlocked,
}

#[derive(Clone, Debug)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub count: i64,
    /// The newest images in the collection, newest first, at most
    /// `COVER_LAYERS` of them. Fewer when the collection is small.
    pub cover_item_ids: Vec<String>,
    pub lock: LockState,
}

/// An album made from the album tags of imported music. Nothing stores it:
/// it exists exactly while at least one of its tracks is in the library, so
/// deleting the last track makes it disappear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    /// Identifies the album across reloads: the album artist (or the artist
    /// when there is none) and the album name, both lowercased.
    pub key: String,
    /// The album name as the first track tags it.
    pub name: String,
    pub count: i64,
    /// The run time of all its tracks together, in milliseconds. Tracks whose
    /// length is unknown count as nothing.
    pub duration_ms: i64,
    /// The album's first tracks in playing order, at most `COVER_LAYERS`.
    pub cover_item_ids: Vec<String>,
}

/// The key of the album a track belongs to, or `None` when it has no album tag.
pub fn album_key(audio: &AudioMeta) -> Option<String> {
    let album = audio.tags.album.as_deref()?.trim();
    if album.is_empty() {
        return None;
    }
    let artist = audio
        .tags
        .album_artist
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or(audio.tags.artist.as_deref())
        .unwrap_or("")
        .trim();
    Some(format!(
        "{}\u{1f}{}",
        artist.to_lowercase(),
        album.to_lowercase()
    ))
}

/// Groups music into albums by their tags, sorted by name. `tracks` is in
/// library order (see `db::music_library`), so each album's first tracks are
/// its opening ones. Tracks with no album tag belong to no album.
pub fn albums<'a>(
    tracks: impl IntoIterator<Item = (&'a str, Option<&'a AudioMeta>)>,
) -> Vec<Album> {
    let mut albums: Vec<Album> = Vec::new();
    let mut slots: HashMap<String, usize> = HashMap::new();
    for (id, audio) in tracks {
        let Some(audio) = audio else { continue };
        let Some(key) = album_key(audio) else {
            continue;
        };
        let slot = match slots.get(&key) {
            Some(&slot) => slot,
            None => {
                albums.push(Album {
                    key: key.clone(),
                    name: audio.tags.album.as_deref().unwrap_or("").trim().to_string(),
                    count: 0,
                    duration_ms: 0,
                    cover_item_ids: Vec::new(),
                });
                slots.insert(key, albums.len() - 1);
                albums.len() - 1
            }
        };
        let album = &mut albums[slot];
        album.count += 1;
        album.duration_ms += audio.duration_ms.unwrap_or(0).max(0);
        if album.cover_item_ids.len() < COVER_LAYERS {
            album.cover_item_ids.push(id.to_string());
        }
    }
    albums.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.key.cmp(&b.key))
    });
    albums
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{Meta, Tags};

    fn track(album: Option<&str>, artist: Option<&str>, album_artist: Option<&str>) -> Meta {
        Meta {
            tags: Tags {
                album: album.map(str::to_string),
                artist: artist.map(str::to_string),
                album_artist: album_artist.map(str::to_string),
                ..Tags::default()
            },
            codec: "mp3".into(),
            bitrate: None,
            sample_rate: None,
            channels: None,
            duration_ms: None,
            has_cover: false,
        }
    }

    #[test]
    fn tracks_without_an_album_tag_belong_to_no_album() {
        assert_eq!(album_key(&track(None, Some("A"), None)), None);
        assert_eq!(album_key(&track(Some("  "), Some("A"), None)), None);
    }

    #[test]
    fn album_artist_wins_over_track_artist_and_case_is_ignored() {
        let feat = track(Some("Live"), Some("Guest"), Some("Band"));
        let plain = track(Some("LIVE"), Some("band"), None);
        assert_eq!(album_key(&feat), album_key(&plain));
        let other = track(Some("Live"), Some("Someone Else"), None);
        assert_ne!(album_key(&feat), album_key(&other));
    }

    #[test]
    fn tracks_group_into_albums_with_their_first_covers_and_counts() {
        let a1 = track(Some("Zed"), Some("X"), Some("X"));
        let a2 = track(Some("Zed"), Some("X"), Some("X"));
        let b1 = track(Some("Alpha"), Some("Y"), None);
        let loose = track(None, Some("Y"), None);
        let tracks = [
            ("t1", Some(&a1)),
            ("t2", Some(&a2)),
            ("t3", Some(&b1)),
            ("t4", Some(&loose)),
            ("t5", None),
        ];
        let albums = albums(tracks.iter().map(|(id, meta)| (*id, *meta)));
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].name, "Alpha");
        assert_eq!(albums[0].count, 1);
        assert_eq!(albums[0].cover_item_ids, vec!["t3"]);
        assert_eq!(albums[1].name, "Zed");
        assert_eq!(albums[1].count, 2);
        assert_eq!(albums[1].cover_item_ids, vec!["t1", "t2"]);
    }

    #[test]
    fn an_album_runs_for_the_sum_of_its_tracks() {
        let mut long = track(Some("Long"), Some("A"), None);
        long.duration_ms = Some(200_000);
        let mut short = track(Some("Long"), Some("A"), None);
        short.duration_ms = Some(61_500);
        let unknown = track(Some("Long"), Some("A"), None);
        let tracks = [
            ("t1", Some(&long)),
            ("t2", Some(&short)),
            ("t3", Some(&unknown)),
        ];
        let albums = albums(tracks.iter().map(|(id, meta)| (*id, *meta)));
        assert_eq!(albums[0].count, 3);
        assert_eq!(albums[0].duration_ms, 261_500);
    }

    #[test]
    fn covers_stop_at_the_layer_limit() {
        let meta = track(Some("Long"), Some("A"), None);
        let ids: Vec<String> = (0..COVER_LAYERS + 2).map(|i| format!("t{i}")).collect();
        let tracks = ids.iter().map(|id| (id.as_str(), Some(&meta)));
        let albums = albums(tracks);
        assert_eq!(albums[0].count as usize, ids.len());
        assert_eq!(albums[0].cover_item_ids.len(), COVER_LAYERS);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub photos: usize,
    pub videos: usize,
    pub audio: usize,
    pub other: usize,
    pub bytes: u64,
    pub original_bytes: u64,
    pub trash_items: usize,
    pub trash_bytes: u64,
    pub pending_thumbs: usize,
    pub missing_thumbs: usize,
    pub optimized_items: usize,
    pub missing_files: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub imported: usize,
    pub duplicates: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes_imported: u64,
    pub bytes_deduplicated: u64,
    pub failures: Vec<(String, String)>,
    /// Ids of the catalogue rows this import added, so an import can be
    /// taken back by removing exactly these.
    pub created: Vec<String>,
}

/// A library snapshot written by [`crate::Library::backup`].
#[derive(Clone, Debug, Default)]
pub struct BackupReport {
    pub path: String,
    pub items: usize,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub checked: usize,
    pub ok: usize,
    pub missing: Vec<String>,
    pub corrupt: Vec<String>,
    pub missing_thumbs: Vec<String>,
    pub orphan_files: usize,
    pub orphan_bytes: u64,
    pub database_ok: bool,
    pub elapsed_ms: u64,
}

impl VerifyReport {
    pub fn is_healthy(&self) -> bool {
        self.missing.is_empty()
            && self.corrupt.is_empty()
            && self.missing_thumbs.is_empty()
            && self.database_ok
    }
}

#[cfg(test)]
mod display_size_tests {
    use super::*;

    fn item(kind: Kind, ext: &str, width: u32, height: u32, orientation: i32) -> Item {
        Item {
            id: String::new(),
            kind,
            mime: String::new(),
            ext: ext.into(),
            original_ext: ext.into(),
            bytes: 0,
            original_bytes: 0,
            width: Some(width),
            height: Some(height),
            duration_ms: None,
            captured_at: 0,
            captured_source: String::new(),
            added_at: 0,
            modified_at: 0,
            original_name: String::new(),
            rel_path: String::new(),
            thumb_state: ThumbState::None,
            opt_state: OptState::None,
            opt_kind: None,
            deleted_at: None,
            camera_make: None,
            camera_model: None,
            lens: None,
            iso: None,
            f_number: None,
            exposure: None,
            focal_mm: None,
            orientation,
            gps: None,
            avg_color: 0,
            checksum: String::new(),
            content_hash: String::new(),
            thumb_recipe: 0,
            thumb_error: None,
            opt_error: None,
        }
    }

    #[test]
    fn a_rotated_jpeg_is_shown_portrait() {
        let portrait = item(Kind::Image, "jpg", 4000, 3000, 6);
        assert_eq!(portrait.display_size(), Some((3000, 4000)));
        assert!((portrait.aspect() - 0.75).abs() < 1e-6);
        let upright = item(Kind::Image, "png", 4000, 3000, 1);
        assert_eq!(upright.display_size(), Some((4000, 3000)));
    }

    #[test]
    fn a_jpeg_xl_keeps_the_original_orientation_for_its_size() {
        let converted = item(Kind::Image, "jxl", 4000, 3000, 8);
        assert_eq!(converted.display_size(), Some((3000, 4000)));
    }

    #[test]
    fn sizes_already_turned_by_the_probe_or_ffmpeg_are_not_turned_again() {
        // A video's probe swapped its size and set the orientation; a HEIC still
        // comes from ffmpeg already rotated. Swapping either again is wrong.
        let video = item(Kind::Video, "mp4", 1080, 1920, 6);
        assert_eq!(video.display_size(), Some((1080, 1920)));
        let heic = item(Kind::Image, "heic", 4032, 3024, 6);
        assert_eq!(heic.display_size(), Some((4032, 3024)));
    }

    #[test]
    fn a_missing_size_has_no_display_size() {
        let mut unknown = item(Kind::Image, "jpg", 1, 1, 6);
        unknown.height = None;
        assert_eq!(unknown.display_size(), None);
        assert!((unknown.aspect() - 1.0).abs() < 1e-6);
    }
}
