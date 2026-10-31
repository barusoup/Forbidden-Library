//! The viewer's state and the rules behind it: which playback engine a file
//! gets, how a still is zoomed and panned, and when the video chrome is on
//! screen. Drawing and input stay in `app.rs`, which owns the GPU and the
//! layout. Everything here that does geometry is a pure function so the rules
//! can be tested without a window.

use crate::media::anim::AnimPlayer;
use crate::media::video::{PlayerStatus, VideoPlayer};
use crate::motion::Spring;
use archive_core::LyricLine;
use dream::texture::GpuTexture;
use dream::ui::Rect;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Frames a replaced texture waits before it is dropped. A frame that was
/// recorded before the swap may still sample it, so it must outlive that frame.
pub const RETIRE_FRAMES: u8 = 3;
/// How fast the zoom eases toward its target, as an exponential rate per second.
pub const ZOOM_RATE: f32 = 18.0;
/// A wheel turn multiplies the zoom target by `exp(delta * WHEEL_ZOOM)`.
pub const WHEEL_ZOOM: f32 = 0.0025;
/// Multiplicative step for the `+` and `-` keys.
pub const KEY_ZOOM_STEP: f32 = 1.5;
/// A still waits this long on screen before its full decode is requested, so
/// that flicking through a folder does not start a decode for every photo.
pub const FULL_DELAY: Duration = Duration::from_millis(250);
/// How long the video bar stays up after the pointer last moved.
pub const CONTROLS_IDLE: Duration = Duration::from_millis(2500);
/// How long the volume slider stays out after the volume is changed by key.
pub const VOLUME_HOLD: Duration = Duration::from_millis(1400);
/// How long the video bar takes to fade in or out.
pub const CONTROLS_FADE: f32 = 0.2;
/// Two presses this close in time and space make a double click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(350);
pub const DOUBLE_CLICK_DISTANCE: f32 = 6.0;
/// How long a full-resolution decode may stay unanswered before another
/// one may be asked for.
pub const FULL_DECODE_TIMEOUT: Duration = Duration::from_secs(30);
/// GPU bytes an animation may keep for later loops.
pub const ANIM_GPU_BUDGET: u64 = 192 * 1024 * 1024;
/// Longest edge an animation frame is decoded to.
pub const ANIM_MAX_SIDE: u32 = 4096;
/// Lowercase extensions the animation engine is asked about. Every other
/// still goes straight to the still path.
pub const ANIM_EXTENSIONS: [&str; 4] = ["gif", "png", "apng", "webp"];
/// How far the Shift+Left and Shift+Right keys seek.
pub const SEEK_STEP_MS: i64 = 5000;
/// Volume change for the Up and Down keys.
pub const VOLUME_STEP: f32 = 0.1;

/// Which engine plays the open item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaKind {
    #[default]
    None,
    Still,
    Anim,
    Video,
}

/// The engine behind the open item. Players are boxed so that a still, which
/// needs none, does not carry the size of the largest one.
pub enum Playback {
    Still,
    Anim(Box<AnimPlayer>),
    Video(Box<VideoPlayer>),
}

/// The media behind the open item: its player and every texture the player
/// has produced for it. Dropping this stops the player without blocking, but
/// the textures must go through [`MediaView::into_retired`] so the GPU keeps
/// them for the frames still in flight.
pub struct MediaView {
    pub id: String,
    pub playback: Playback,
    /// The animation or video frame on screen.
    pub frame: Option<Arc<GpuTexture>>,
    /// Animation frames by index, kept while they fit the GPU budget.
    pub anim_cache: Vec<Option<Arc<GpuTexture>>>,
    /// Full-resolution still, decoded after the preview is already on screen.
    pub full: Option<Arc<GpuTexture>>,
    /// Earliest moment a full decode may be asked for.
    pub full_wanted_at: std::time::Instant,
    /// Textures replaced while a frame may still sample them.
    pub retired: Vec<(Arc<GpuTexture>, u8)>,
    /// Frames put on screen so far. Lets the rendercheck see playback move.
    pub frames_shown: u64,
    /// Synced lyrics of a song, in time order. Empty for anything else.
    pub lyrics: Vec<LyricLine>,
}

impl MediaView {
    pub fn new(id: String, playback: Playback, now: Instant) -> Self {
        Self {
            id,
            playback,
            frame: None,
            anim_cache: Vec::new(),
            full: None,
            full_wanted_at: now + FULL_DELAY,
            retired: Vec::new(),
            frames_shown: 0,
            lyrics: Vec::new(),
        }
    }

    fn retire(&mut self, texture: Arc<GpuTexture>) {
        self.retired.push((texture, RETIRE_FRAMES));
    }

    pub fn set_frame(&mut self, texture: Arc<GpuTexture>) {
        if let Some(old) = self.frame.replace(texture) {
            self.retire(old);
        }
        self.frames_shown += 1;
    }

    pub fn clear_frame(&mut self) {
        if let Some(old) = self.frame.take() {
            self.retire(old);
        }
    }

    pub fn set_cached(&mut self, index: usize, texture: Arc<GpuTexture>) {
        if self.anim_cache.len() <= index {
            self.anim_cache.resize(index + 1, None);
        }
        if let Some(old) = self.anim_cache[index].replace(texture) {
            self.retire(old);
        }
    }

    pub fn clear_cache(&mut self) {
        for entry in std::mem::take(&mut self.anim_cache).into_iter().flatten() {
            self.retire(entry);
        }
    }

    pub fn set_full(&mut self, texture: Arc<GpuTexture>) {
        if let Some(old) = self.full.replace(texture) {
            self.retire(old);
        }
    }

    /// Once per rendered frame, like [`crate::thumbs::ThumbCache::retire_frame`].
    pub fn retire_frame(&mut self) {
        for (_, frames) in self.retired.iter_mut() {
            *frames = frames.saturating_sub(1);
        }
        self.retired.retain(|(_, frames)| *frames > 0);
    }

    /// Every texture this view still owns, each with a fresh countdown, so the
    /// caller can hand them to the app's retired list before the player drops.
    pub fn into_retired(mut self) -> Vec<(Arc<GpuTexture>, u8)> {
        let mut out = std::mem::take(&mut self.retired);
        let mut all: Vec<Arc<GpuTexture>> = Vec::new();
        all.extend(self.frame.take());
        all.extend(self.full.take());
        all.extend(std::mem::take(&mut self.anim_cache).into_iter().flatten());
        out.extend(all.into_iter().map(|texture| (texture, RETIRE_FRAMES)));
        out
    }

    pub fn video(&self) -> Option<&VideoPlayer> {
        match &self.playback {
            Playback::Video(player) => Some(player.as_ref()),
            _ => None,
        }
    }

    pub fn video_mut(&mut self) -> Option<&mut VideoPlayer> {
        match &mut self.playback {
            Playback::Video(player) => Some(player.as_mut()),
            _ => None,
        }
    }

    pub fn kind(&self) -> MediaKind {
        match &self.playback {
            Playback::Still => MediaKind::Still,
            Playback::Anim(_) => MediaKind::Anim,
            Playback::Video(_) => MediaKind::Video,
        }
    }
}

/// Which way a zoom key asks the view to move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomStep {
    In,
    Out,
    Reset,
}

/// The point of the image that stays under the pointer while a zoom eases.
/// `cursor` is in logical pixels; `point` is in normalised image coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pivot {
    pub cursor: (f32, f32),
    pub point: (f32, f32),
}

/// Zoom and pan of a still or animation. `zoom` is what is drawn and eases
/// toward `target`. `center` is the normalised point of the image at the
/// centre of the media area, in 0..1 on each axis.
///
/// `pivot` is set while a zoom is easing: the centre is re-solved every frame
/// from the pivot, so the point under the pointer stays put throughout the
/// ease rather than sliding to where a single jump would have put it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTransform {
    pub zoom: f32,
    pub target: f32,
    pub center: (f32, f32),
    pub pivot: Option<Pivot>,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            target: 1.0,
            center: (0.5, 0.5),
            pivot: None,
        }
    }
}

/// The open viewer: the list it browses, the index into it, and the chrome and
/// gesture state that persists across items.
pub struct Viewer {
    pub ids: Vec<String>,
    pub index: usize,
    pub open: f32,
    pub closing: bool,
    pub info: Spring,
    pub info_open: bool,
    pub media: Option<MediaView>,
    pub view: ViewTransform,
    /// The video bar stays up until this moment, unless something holds it.
    pub controls_until: Instant,
    /// How far the video bar is shown, 0 hidden to 1 shown, and where it is
    /// heading. The bar fades toward the goal over [`CONTROLS_FADE`].
    pub controls: f32,
    pub controls_goal: f32,
    /// The bar as drawn last frame, so the pointer can hold it open.
    pub bar: Rect,
    /// The last press on the image, for double-click detection.
    pub last_click: Option<(Instant, (f32, f32))>,
    /// Where a pan drag last saw the pointer.
    pub drag: Option<(f32, f32)>,
    pub scrubbing: bool,
    pub volume_drag: bool,
    /// How far the volume slider is out, 0 to 1. It opens under the pointer
    /// and for a moment after the volume changes any other way.
    pub volume_open: f32,
    pub volume_hold_until: Instant,
    /// The volume as drawn, which eases to the player's when it changes
    /// without the slider being dragged.
    pub volume_shown: Spring,
    /// 0 for the play icon to 1 for the pause icon, as drawn.
    pub play_shown: f32,
    /// How much of the speaker's low wave, high wave and cross are showing.
    pub speaker_shown: [f32; 3],
    /// The volume slider's zone as drawn last frame.
    pub volume_zone: Rect,
    /// Whether the controls' own animation is still moving.
    pub chrome_busy: bool,
    /// A zoom key pressed since the last frame. Applied in drawing, where the
    /// media area is known.
    pub zoom_step: Option<ZoomStep>,
}

impl Viewer {
    pub fn new(ids: Vec<String>, index: usize, now: Instant) -> Self {
        Self {
            ids,
            index,
            open: 0.0,
            closing: false,
            info: Spring::at(0.0),
            info_open: false,
            media: None,
            view: ViewTransform::default(),
            controls_until: now + CONTROLS_IDLE,
            controls: 0.0,
            controls_goal: 0.0,
            bar: Rect::default(),
            last_click: None,
            drag: None,
            scrubbing: false,
            volume_drag: false,
            volume_open: 0.0,
            volume_hold_until: now,
            volume_shown: Spring::at(1.0),
            play_shown: 0.0,
            speaker_shown: [1.0, 1.0, 0.0],
            volume_zone: Rect::default(),
            chrome_busy: false,
            zoom_step: None,
        }
    }

    /// Forget everything tied to the previous item. Chrome that is not tied to
    /// an item (the Details panel) stays as the user left it.
    pub fn reset_for_item(&mut self, now: Instant) {
        self.view = ViewTransform::default();
        self.drag = None;
        self.scrubbing = false;
        self.volume_drag = false;
        self.last_click = None;
        self.zoom_step = None;
        self.controls_until = now + CONTROLS_IDLE;
    }
}

/// The normalised scale of the fitted image at zoom 1, in logical pixels.
fn fit_size(area: Rect, aspect: f32) -> (f32, f32) {
    crate::app::fit_in(area, aspect).size()
}

/// The drawn size of the image at the current zoom.
fn scaled_size(area: Rect, aspect: f32, zoom: f32) -> (f32, f32) {
    let (w, h) = fit_size(area, aspect);
    (w * zoom, h * zoom)
}

/// Clamp a centre on one axis. An image larger than the area may be panned
/// until its edge meets the area's edge; a smaller one stays centred.
fn clamp_axis(centre: f32, area_len: f32, size: f32) -> f32 {
    if size <= area_len {
        return 0.5;
    }
    let half = area_len / (2.0 * size);
    centre.clamp(half, 1.0 - half)
}

/// Keep the centre in the range where the image still covers the area.
pub fn clamp_center(center: (f32, f32), area: Rect, aspect: f32, zoom: f32) -> (f32, f32) {
    let (sw, sh) = scaled_size(area, aspect, zoom);
    (
        clamp_axis(center.0, area.w, sw),
        clamp_axis(center.1, area.h, sh),
    )
}

/// Where the image is drawn: the letterbox fit, scaled by the zoom, with the
/// centre at the middle of the area and the edges kept inside the area.
pub fn image_rect(area: Rect, aspect: f32, view: &ViewTransform) -> Rect {
    let (sw, sh) = scaled_size(area, aspect, view.zoom);
    let (cx, cy) = clamp_center(view.center, area, aspect, view.zoom);
    Rect::new(
        area.x + area.w * 0.5 - cx * sw,
        area.y + area.h * 0.5 - cy * sh,
        sw,
        sh,
    )
}

/// The centre that keeps `pivot.point` under `pivot.cursor` at `zoom`. With
/// the image laid out as in [`image_rect`], the point sits at
/// `area_centre - centre * size + point * size`, so solving for the centre
/// gives `point + (area_centre - cursor) / size`.
fn centre_for_pivot(area: Rect, aspect: f32, zoom: f32, pivot: Pivot) -> (f32, f32) {
    let (sw, sh) = scaled_size(area, aspect, zoom);
    let ax = area.x + area.w * 0.5;
    let ay = area.y + area.h * 0.5;
    (
        pivot.point.0 + (ax - pivot.cursor.0) / sw.max(1e-3),
        pivot.point.1 + (ay - pivot.cursor.1) / sh.max(1e-3),
    )
}

/// Clamp a requested zoom into the range the still may use.
pub fn clamp_zoom(zoom: f32, max: f32) -> f32 {
    zoom.clamp(1.0, max.max(1.0))
}

/// The zoom at which one image pixel lands on one physical pixel. The fit is
/// in logical pixels, so the scale turns it into device pixels first.
pub fn actual_zoom(native_long: f32, fit: (f32, f32), scale: f32) -> f32 {
    native_long / (fit.0.max(fit.1) * scale.max(0.5)).max(1.0)
}

/// Largest zoom allowed: four times the pixel-for-pixel zoom, and never less
/// than 2x, so a small image can still be magnified a little.
pub fn max_zoom(actual: f32) -> f32 {
    (actual * 4.0).max(2.0)
}

/// Set the zoom target, anchored at `cursor`. The zoom itself eases in
/// [`ease_view`], which keeps the anchor under the pointer as it goes.
pub fn zoom_at(
    view: ViewTransform,
    area: Rect,
    aspect: f32,
    new_target: f32,
    cursor: (f32, f32),
) -> ViewTransform {
    let rect = image_rect(area, aspect, &view);
    let point = (
        (cursor.0 - rect.x) / rect.w.max(1e-3),
        (cursor.1 - rect.y) / rect.h.max(1e-3),
    );
    ViewTransform {
        target: new_target.max(1.0),
        pivot: Some(Pivot { cursor, point }),
        ..view
    }
}

/// Move the zoom a step toward its target. The step is frame-rate
/// independent, and the zoom snaps once it is within a thousandth.
pub fn ease_view(view: ViewTransform, area: Rect, aspect: f32, dt: f32) -> ViewTransform {
    let step = 1.0 - (-dt.max(0.0) * ZOOM_RATE).exp();
    let mut zoom = view.zoom + (view.target - view.zoom) * step;
    if (zoom - view.target).abs() <= 0.001 {
        zoom = view.target;
    }
    let mut next = ViewTransform { zoom, ..view };
    if let Some(pivot) = view.pivot {
        next.center = centre_for_pivot(area, aspect, zoom, pivot);
        if zoom == view.target {
            next.pivot = None;
        }
    }
    next.center = clamp_center(next.center, area, aspect, zoom);
    next
}

/// Drag the image by a pointer movement in logical pixels. A pan takes over
/// from any zoom still anchored to a pivot.
pub fn pan(view: ViewTransform, area: Rect, aspect: f32, dx: f32, dy: f32) -> ViewTransform {
    let (sw, sh) = scaled_size(area, aspect, view.zoom);
    let center = (
        view.center.0 - dx / sw.max(1e-3),
        view.center.1 - dy / sh.max(1e-3),
    );
    ViewTransform {
        center: clamp_center(center, area, aspect, view.zoom),
        pivot: None,
        ..view
    }
}

/// Whether two presses form a double click.
pub fn is_double_click(
    previous: Option<(Instant, (f32, f32))>,
    now: Instant,
    at: (f32, f32),
) -> bool {
    let Some((when, from)) = previous else {
        return false;
    };
    let dx = at.0 - from.0;
    let dy = at.1 - from.1;
    now.saturating_duration_since(when) <= DOUBLE_CLICK
        && (dx * dx + dy * dy).sqrt() <= DOUBLE_CLICK_DISTANCE
}

/// Whether the video bar should be up. It stays up while the video is not
/// playing (there is then something to act on), for a while after the pointer
/// moves, and while the pointer is over it.
pub fn controls_wanted(status: &PlayerStatus, now: Instant, until: Instant, hovered: bool) -> bool {
    !matches!(status, PlayerStatus::Playing) || now < until || hovered
}

/// Where the video controls sit. They have no container: a play button, the
/// progress track, and the volume icon, which reveals its slider to its right.
/// The row is centred on the media area, just above the action pill.
pub struct BarLayout {
    /// The whole row, which holds the controls up while the pointer is on it.
    pub bar: Rect,
    pub play: Rect,
    pub track: Rect,
    pub mute: Rect,
    /// The slider, which the bar's right edge wipes in from the left as it opens.
    pub volume_track: Rect,
    /// The pointer area that opens the slider: the icon, plus the slider once
    /// it is out.
    pub volume: Rect,
}

pub const BAR_H: f32 = 40.0;
pub const BAR_MAX_W: f32 = 720.0;
pub const VOLUME_W: f32 = 84.0;
/// The side of the play and volume icons' hit areas, and of their 24 grid.
pub const BAR_ICON: f32 = 32.0;

/// `height` is the window's logical height; the pill's top sits at
/// `height - 76`, and the row keeps an 8 px gap above it. `volume_open` is
/// how far the volume slider is out, 0 to 1; the progress track gives it room.
pub fn bar_layout(area: Rect, height: f32, volume_open: f32) -> BarLayout {
    let open = volume_open.clamp(0.0, 1.0);
    let w = area.w.min(BAR_MAX_W);
    let x = area.x + (area.w - w) * 0.5;
    let y = height - 76.0 - 8.0 - BAR_H;
    let bar = Rect::new(x, y, w, BAR_H);
    let gap = 10.0;
    let icon_y = y + (BAR_H - BAR_ICON) * 0.5;
    let play = Rect::new(x, icon_y, BAR_ICON, BAR_ICON);
    // The icon slides left to make room as the slider comes out to its right.
    let mute = Rect::new(
        x + w - BAR_ICON - (VOLUME_W + gap) * open,
        icon_y,
        BAR_ICON,
        BAR_ICON,
    );
    let volume_track = Rect::new(mute.x + BAR_ICON + gap, y, VOLUME_W, BAR_H);
    let volume = if open > 0.05 {
        Rect::new(mute.x, y, x + w - mute.x, BAR_H)
    } else {
        mute
    };
    let track_x = play.x + BAR_ICON + gap;
    let track_end = mute.x - gap;
    let track = Rect::new(track_x, y, (track_end - track_x).max(60.0), BAR_H);
    BarLayout {
        bar,
        play,
        track,
        mute,
        volume_track,
        volume,
    }
}

/// Where along a track a pointer position lies, from 0 to 1.
pub fn fraction_along(track: Rect, x: f32) -> f32 {
    ((x - track.x) / track.w.max(1e-3)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "{a} != {b}");
    }

    fn area() -> Rect {
        Rect::new(80.0, 92.0, 800.0, 600.0)
    }

    #[test]
    fn a_landscape_still_fits_the_width_and_centres_vertically() {
        let rect = image_rect(area(), 2.0, &ViewTransform::default());
        close(rect.w, 800.0);
        close(rect.h, 400.0);
        close(rect.x, 80.0);
        close(rect.y, 92.0 + 100.0);
    }

    #[test]
    fn a_portrait_still_fits_the_height_and_centres_horizontally() {
        let rect = image_rect(area(), 0.5, &ViewTransform::default());
        close(rect.h, 600.0);
        close(rect.w, 300.0);
        close(rect.x, 80.0 + 250.0);
        close(rect.y, 92.0);
    }

    #[test]
    fn a_zoomed_image_clamps_to_the_area_on_both_axes() {
        // A square at zoom 2 is 1200 px on each side in a 800x600 area.
        let view = ViewTransform {
            zoom: 2.0,
            target: 2.0,
            center: (0.0, 0.0),
            pivot: None,
        };
        let rect = image_rect(area(), 1.0, &view);
        close(rect.w, 1200.0);
        close(rect.h, 1200.0);
        // The top-left corner of the image may not come inside the area.
        close(rect.x, 80.0);
        close(rect.y, 92.0);

        let view = ViewTransform {
            center: (1.0, 1.0),
            ..view
        };
        let rect = image_rect(area(), 1.0, &view);
        close(rect.x + rect.w, 80.0 + 800.0);
        close(rect.y + rect.h, 92.0 + 600.0);
    }

    #[test]
    fn a_zoomed_image_still_centres_on_an_axis_it_does_not_overflow() {
        // A wide image at zoom 2 overflows horizontally, but its height is
        // still inside the 600 px area, so it stays centred vertically.
        let view = ViewTransform {
            zoom: 2.0,
            target: 2.0,
            center: (0.5, 0.9),
            pivot: None,
        };
        let rect = image_rect(area(), 3.0, &view);
        close(rect.h, 2.0 * 800.0 / 3.0);
        close(rect.y, 92.0 + (600.0 - rect.h) * 0.5);
    }

    #[test]
    fn the_point_under_the_cursor_stays_put_while_zooming() {
        let view = ViewTransform::default();
        let cursor = (600.0, 400.0);
        let before = image_rect(area(), 1.0, &view);
        let point = (
            (cursor.0 - before.x) / before.w,
            (cursor.1 - before.y) / before.h,
        );
        let zoomed = zoom_at(view, area(), 1.0, 3.0, cursor);
        // A large dt lets the ease finish in one step.
        let settled = ease_view(zoomed, area(), 1.0, 10.0);
        assert_eq!(settled.zoom, 3.0);
        let after = image_rect(area(), 1.0, &settled);
        let under = (cursor.0 - after.x, cursor.1 - after.y);
        assert!((under.0 - point.0 * after.w).abs() < 0.5);
        assert!((under.1 - point.1 * after.h).abs() < 0.5);
    }

    #[test]
    fn the_point_under_the_cursor_stays_put_mid_ease() {
        let view = ViewTransform::default();
        let cursor = (620.0, 480.0);
        let before = image_rect(area(), 1.0, &view);
        let point = (
            (cursor.0 - before.x) / before.w,
            (cursor.1 - before.y) / before.h,
        );
        let mut eased = zoom_at(view, area(), 1.0, 4.0, cursor);
        for _ in 0..5 {
            eased = ease_view(eased, area(), 1.0, 1.0 / 60.0);
            assert!(eased.zoom > 1.0 && eased.zoom < 4.0);
            let rect = image_rect(area(), 1.0, &eased);
            let under = (cursor.0 - rect.x, cursor.1 - rect.y);
            assert!((under.0 - point.0 * rect.w).abs() < 0.5);
            assert!((under.1 - point.1 * rect.h).abs() < 0.5);
        }
    }

    #[test]
    fn zoom_is_clamped_to_fit_and_to_the_maximum() {
        assert_eq!(clamp_zoom(0.2, 8.0), 1.0);
        assert_eq!(clamp_zoom(50.0, 8.0), 8.0);
        // Pointing the zoom past the limit clamps the target, not the centre.
        let view = zoom_at(ViewTransform::default(), area(), 1.0, 0.2, (400.0, 300.0));
        assert_eq!(view.target, 1.0);
        let view = ease_view(
            ViewTransform {
                target: 1e9_f32.min(max_zoom(1.0)),
                ..ViewTransform::default()
            },
            area(),
            1.0,
            10.0,
        );
        assert_eq!(view.zoom, max_zoom(1.0));
        // The square is 600 px at fit, so its width at this zoom bounds the pan.
        let half = 800.0 / (2.0 * 600.0 * view.zoom);
        assert!(view.center.0 >= half - 1e-4 && view.center.0 <= 1.0 - half + 1e-4);
    }

    #[test]
    fn the_maximum_zoom_follows_the_native_pixels() {
        // A 6000 px image shown at 600 logical px is 10x its fit size at 1x.
        let actual = actual_zoom(6000.0, (600.0, 400.0), 1.0);
        close(actual, 10.0);
        close(max_zoom(actual), 40.0);
        // A small image still gets at least 2x.
        close(max_zoom(actual_zoom(200.0, (600.0, 400.0), 1.0)), 2.0);
        // Physical pixels: at 2x scale the same image is half as magnified.
        close(actual_zoom(6000.0, (600.0, 400.0), 2.0), 5.0);
    }

    #[test]
    fn a_pan_moves_the_image_with_the_pointer_and_stops_at_the_edge() {
        let view = ViewTransform {
            zoom: 2.0,
            target: 2.0,
            center: (0.5, 0.5),
            pivot: None,
        };
        let moved = pan(view, area(), 1.0, 100.0, 0.0);
        let before = image_rect(area(), 1.0, &view);
        let after = image_rect(area(), 1.0, &moved);
        close(after.x - before.x, 100.0);
        // Dragging far beyond the edge stops with the edge at the area's edge.
        let far = pan(view, area(), 1.0, 10_000.0, 0.0);
        close(image_rect(area(), 1.0, &far).x, 80.0);
    }

    #[test]
    fn two_close_presses_are_a_double_click() {
        let now = Instant::now();
        let first = Some((now, (100.0, 100.0)));
        assert!(is_double_click(
            first,
            now + Duration::from_millis(200),
            (103.0, 102.0)
        ));
        assert!(!is_double_click(
            first,
            now + Duration::from_millis(400),
            (100.0, 100.0)
        ));
        assert!(!is_double_click(
            first,
            now + Duration::from_millis(100),
            (110.0, 100.0)
        ));
        assert!(!is_double_click(None, now, (100.0, 100.0)));
    }

    #[test]
    fn the_bar_is_centred_above_the_pill_and_its_parts_do_not_overlap() {
        for open in [0.0, 1.0] {
            let layout = bar_layout(area(), 900.0, open);
            assert!(layout.play.x + layout.play.w <= layout.track.x);
            assert!(layout.track.x + layout.track.w <= layout.volume.x);
            assert!(layout.volume.x + layout.volume.w <= layout.bar.x + layout.bar.w + 0.01);
        }
        let layout = bar_layout(area(), 900.0, 1.0);
        close(layout.bar.w, BAR_MAX_W.min(800.0));
        close(layout.bar.y + layout.bar.h, 900.0 - 76.0 - 8.0);
        close(layout.bar.x + layout.bar.w * 0.5, area().x + area().w * 0.5);
    }
}
