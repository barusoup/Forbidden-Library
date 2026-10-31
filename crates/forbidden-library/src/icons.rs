//! The app's icons, vendored from [Lucide](https://lucide.dev) at
//! lucide-static 0.544.0.
//!
//! Every glyph is Lucide's own SVG geometry on their 24x24 grid with the
//! default 2px stroke, round caps and round joins. BedFrame rasterizes the
//! paths into the UI atlas on demand, so they anti-alias at any DPI and tint
//! like text. Nothing here is hand-drawn.

use dream::ui::widgets::Ui;
use dream::ui::{Color, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppIcon {
    Photos,
    Collections,
    Trash,
    Search,
    SearchX,
    Info,
    Back,
    Forward,
    Play,
    Check,
    CircleMinus,
    Close,
    Download,
    Upload,
    FolderOpen,
    Smartphone,
    Aperture,
    Calendar,
    FileImage,
    MapPin,
    ChevronDown,
    ChevronUp,
    Plus,
    Pause,
    VolumeBody,
    VolumeWaveLow,
    VolumeWaveHigh,
    VolumeCross,
    LoaderCircle,
    Music,
    Lock,
    LockOpen,
}

/// The Lucide slug, used as the atlas cache key and for provenance.
pub fn slug(icon: AppIcon) -> &'static str {
    match icon {
        AppIcon::Photos => "images",
        AppIcon::Collections => "album",
        AppIcon::Trash => "trash-2",
        AppIcon::Search => "search",
        AppIcon::SearchX => "search-x",
        AppIcon::Info => "info",
        AppIcon::Back => "chevron-left",
        AppIcon::Forward => "chevron-right",
        AppIcon::Play => "play",
        AppIcon::Check => "check",
        AppIcon::CircleMinus => "circle-minus",
        AppIcon::Close => "x",
        AppIcon::Download => "download",
        AppIcon::Upload => "upload",
        AppIcon::FolderOpen => "folder-open",
        AppIcon::Smartphone => "smartphone",
        AppIcon::Aperture => "aperture",
        AppIcon::Calendar => "calendar",
        AppIcon::FileImage => "file-image",
        AppIcon::MapPin => "map-pin",
        AppIcon::ChevronDown => "chevron-down",
        AppIcon::ChevronUp => "chevron-up",
        AppIcon::Plus => "plus",
        AppIcon::Pause => "pause",
        AppIcon::VolumeBody => "volume-body",
        AppIcon::VolumeWaveLow => "volume-wave-low",
        AppIcon::VolumeWaveHigh => "volume-wave-high",
        AppIcon::VolumeCross => "volume-cross",
        AppIcon::LoaderCircle => "loader-circle",
        AppIcon::Music => "music",
        AppIcon::Lock => "lock",
        AppIcon::LockOpen => "lock-open",
    }
}

fn paths(icon: AppIcon) -> &'static [&'static str] {
    match icon {
        AppIcon::Photos => &[
            "m22 11-1.296-1.296a2.4 2.4 0 0 0-3.408 0L11 16",
            "M4 8a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2",
            "M12.0 7.0a1.0 1.0 0 1 0 2.0 0a1.0 1.0 0 1 0 -2.0 0",
            "M10.0 2.0H20.0A2.0 2.0 0 0 1 22.0 4.0V14.0A2.0 2.0 0 0 1 20.0 16.0H10.0A2.0 2.0 0 0 1 8.0 14.0V4.0A2.0 2.0 0 0 1 10.0 2.0Z",
        ],
        AppIcon::Collections => &[
            "M5.0 3.0H19.0A2.0 2.0 0 0 1 21.0 5.0V19.0A2.0 2.0 0 0 1 19.0 21.0H5.0A2.0 2.0 0 0 1 3.0 19.0V5.0A2.0 2.0 0 0 1 5.0 3.0Z",
            "M11 3L11 11L14 8L17 11L17 3",
        ],
        AppIcon::Trash => &[
            "M10 11v6",
            "M14 11v6",
            "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6",
            "M3 6h18",
            "M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2",
        ],
        AppIcon::Search => &[
            "m21 21-4.34-4.34",
            "M3.0 11.0a8.0 8.0 0 1 0 16.0 0a8.0 8.0 0 1 0 -16.0 0",
        ],
        AppIcon::SearchX => &[
            "m13.5 8.5-5 5",
            "m8.5 8.5 5 5",
            "M3.0 11.0a8.0 8.0 0 1 0 16.0 0a8.0 8.0 0 1 0 -16.0 0",
            "m21 21-4.3-4.3",
        ],
        AppIcon::Info => &[
            "M2.0 12.0a10.0 10.0 0 1 0 20.0 0a10.0 10.0 0 1 0 -20.0 0",
            "M12 16v-4",
            "M12 8h.01",
        ],
        AppIcon::Back => &["m15 18-6-6 6-6"],
        AppIcon::Forward => &["m9 18 6-6-6-6"],
        AppIcon::Play => {
            &["M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z"]
        }
        AppIcon::Check => &["M20 6 9 17l-5-5"],
        AppIcon::Plus => &["M5 12h14", "M12 5v14"],
        AppIcon::CircleMinus => &[
            "M2.0 12.0a10.0 10.0 0 1 0 20.0 0a10.0 10.0 0 1 0 -20.0 0",
            "M8 12h8",
        ],
        AppIcon::Close => &["M18 6 6 18", "m6 6 12 12"],
        AppIcon::Download => &[
            "M12 15V3",
            "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4",
            "m7 10 5 5 5-5",
        ],
        AppIcon::Upload => &[
            "M12 3v12",
            "m17 8-5-5-5 5",
            "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4",
        ],
        // Lucide 0.544.0, ISC. Source SVGs: assets/icons/lucide/.
        // The smartphone rect was converted to its equivalent path by svgelements.
        AppIcon::FolderOpen => &[
            "m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2",
        ],
        AppIcon::Smartphone => &[
            "M 7,2 L 17,2 A 2,2 0 0,1 19,4 L 19,20 A 2,2 0 0,1 17,22 L 7,22 A 2,2 0 0,1 5,20 L 5,4 A 2,2 0 0,1 7,2 Z",
            "M12 18h.01",
        ],
        AppIcon::Aperture => &[
            "M2.0 12.0a10.0 10.0 0 1 0 20.0 0a10.0 10.0 0 1 0 -20.0 0",
            "m14.31 8 5.74 9.94",
            "M9.69 8h11.48",
            "m7.38 12 5.74-9.94",
            "M9.69 16 3.95 6.06",
            "M14.31 16H2.83",
            "m16.62 12-5.74 9.94",
        ],
        AppIcon::Calendar => &[
            "M8 2v3",
            "M16 2v3",
            "M5 3H19A2 2 0 0 1 21 5V19A2 2 0 0 1 19 21H5A2 2 0 0 1 3 19V5A2 2 0 0 1 5 3Z",
            "M3 9h18",
        ],
        AppIcon::FileImage => &[
            "M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z",
            "M14 2v5a1 1 0 0 0 1 1h5",
            "M8 12a2 2 0 1 0 4 0a2 2 0 1 0 -4 0",
            "m20 17-1.296-1.296a2.41 2.41 0 0 0-3.408 0L9 22",
        ],
        AppIcon::MapPin => &[
            "M20 10c0 4.993-5.539 10.193-7.399 11.799a1 1 0 0 1-1.202 0C9.539 20.193 4 14.993 4 10a8 8 0 0 1 16 0",
            "M9 10a3 3 0 1 0 6 0a3 3 0 1 0 -6 0",
        ],
        AppIcon::ChevronDown => &["m6 9 6 6 6-6"],
        AppIcon::ChevronUp => &["m18 15-6-6-6 6"],
        // The two bars are Lucide's rects with rx 1, written as paths.
        AppIcon::Pause => &[
            "M15 3H18A1 1 0 0 1 19 4V20A1 1 0 0 1 18 21H15A1 1 0 0 1 14 20V4A1 1 0 0 1 15 3Z",
            "M6 3H9A1 1 0 0 1 10 4V20A1 1 0 0 1 9 21H6A1 1 0 0 1 5 20V4A1 1 0 0 1 6 3Z",
        ],
        // Lucide's volume-2 and volume-x, split into the speaker and the marks
        // beside it. They share the 24 grid, so layered they are the whole icon.
        AppIcon::VolumeBody => &[
            "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z",
        ],
        AppIcon::VolumeWaveLow => &["M16 9a5 5 0 0 1 0 6"],
        AppIcon::VolumeWaveHigh => &["M19.364 18.364a9 9 0 0 0 0-12.728"],
        AppIcon::VolumeCross => &["M22 9L16 15", "M16 9L22 15"],
        AppIcon::LoaderCircle => &["M21 12a9 9 0 1 1-6.219-8.56"],
        // Lucide's circles, written as arcs like the rest of this table.
        AppIcon::Music => &[
            "M9 18V5l12-2v13",
            "M3.0 18.0a3.0 3.0 0 1 0 6.0 0a3.0 3.0 0 1 0 -6.0 0",
            "M15.0 16.0a3.0 3.0 0 1 0 6.0 0a3.0 3.0 0 1 0 -6.0 0",
        ],
        // Lucide `lock` and `lock-open` (0.544.0): the body is Lucide's rounded
        // rect, written as the path it converts to.
        AppIcon::Lock => &[
            "M5.0 11.0H19.0A2.0 2.0 0 0 1 21.0 13.0V20.0A2.0 2.0 0 0 1 19.0 22.0H5.0A2.0 2.0 0 0 1 3.0 20.0V13.0A2.0 2.0 0 0 1 5.0 11.0Z",
            "M7 11V7a5 5 0 0 1 10 0v4",
        ],
        AppIcon::LockOpen => &[
            "M5.0 11.0H19.0A2.0 2.0 0 0 1 21.0 13.0V20.0A2.0 2.0 0 0 1 19.0 22.0H5.0A2.0 2.0 0 0 1 3.0 20.0V13.0A2.0 2.0 0 0 1 5.0 11.0Z",
            "M7 11V7a5 5 0 0 1 9.9-1",
        ],
    }
}

/// Draw `icon` centred in `rect` at the rect's short side, tinted `color`.
pub fn paint(ui: &mut Ui, rect: Rect, icon: AppIcon, color: Color) {
    if color.a == 0 || rect.w < 2.0 || rect.h < 2.0 {
        return;
    }
    let size = rect.w.min(rect.h).round().max(1.0) as u32;
    ui.custom_icon(slug(icon), rect, paths(icon), size, color);
}

/// Draw `icon` on its raw 24 grid in `rect`, with no ink fitting, so icons
/// that share a grid sit exactly alike and can be layered. `bake` is the side
/// it is rasterized at; `rect` may be scaled from it for free. `fill` fills the
/// closed shapes, as Lucide's glyphs take when set to `currentColor`.
pub fn paint_grid(ui: &mut Ui, rect: Rect, bake: f32, icon: AppIcon, fill: bool, color: Color) {
    if color.a == 0 || rect.w < 1.0 || rect.h < 1.0 {
        return;
    }
    let stroke = line_weight(fill);
    let name = format!("{}-{fill}-{stroke}", slug(icon));
    ui.grid_icon(&name, rect, bake, paths(icon), fill, stroke, color);
}

/// Draw `icon` as `paint_grid` does, under a halo of `color` that reaches
/// `outline` grid units beyond the glyph on every side. The halo is the same
/// geometry with a wider stroke, so the outline is even all round and costs
/// one draw rather than several offset copies that would stack their alpha.
#[cfg(not(feature = "plain-controls"))]
pub fn paint_grid_outlined(
    ui: &mut Ui,
    rect: Rect,
    bake: f32,
    icon: AppIcon,
    fill: bool,
    color: Color,
    outline: f32,
) {
    if color.a == 0 || rect.w < 1.0 || rect.h < 1.0 {
        return;
    }
    // The stroke is centred on the path, so each side grows by half of it.
    let stroke = line_weight(fill) + 2.0 * outline;
    let name = format!("{}-{fill}-{stroke}", slug(icon));
    ui.grid_icon(&name, rect, bake, paths(icon), fill, stroke, color);
}

/// The stroke weight on the 24 grid. A filled shape keeps only a hairline of
/// stroke, to round its corners; Lucide's full 2px on top of a fill reads
/// bulky at small sizes. The marks beside the speaker are lighter than the
/// default to sit with it.
fn line_weight(fill: bool) -> f32 {
    if fill { 1.0 } else { 1.6 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_lucide_geometry() {
        for icon in [
            AppIcon::Photos,
            AppIcon::Collections,
            AppIcon::Trash,
            AppIcon::Search,
            AppIcon::SearchX,
            AppIcon::Info,
            AppIcon::Back,
            AppIcon::Forward,
            AppIcon::Play,
            AppIcon::Check,
            AppIcon::CircleMinus,
            AppIcon::Close,
            AppIcon::Download,
            AppIcon::Upload,
            AppIcon::FolderOpen,
            AppIcon::Smartphone,
            AppIcon::Aperture,
            AppIcon::Calendar,
            AppIcon::FileImage,
            AppIcon::MapPin,
            AppIcon::ChevronDown,
            AppIcon::ChevronUp,
            AppIcon::Pause,
            AppIcon::VolumeBody,
            AppIcon::VolumeWaveLow,
            AppIcon::VolumeWaveHigh,
            AppIcon::VolumeCross,
            AppIcon::LoaderCircle,
            AppIcon::Lock,
            AppIcon::LockOpen,
        ] {
            assert!(!paths(icon).is_empty(), "{} has no paths", slug(icon));
        }
    }
}
