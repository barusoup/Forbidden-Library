//! The application UI. Everything below is drawn immediately each frame on
//! top of BedFrame's draw list; the archive engine is only ever reached
//! through background workers (except for tiny catalogue reads).

use crate::details;
use crate::grid::{self, GridLayout, HEADER_H, TILE_TARGET};
#[cfg(not(feature = "plain-controls"))]
use crate::icons::paint_grid_outlined;
use crate::icons::{AppIcon, paint as paint_icon, paint_grid};
use crate::media::anim::{AnimPlayer, AnimStatus};
use crate::media::video::{PlayerStatus, VideoPlayer};
use crate::media::{DecodedFrame, Wake};
use crate::motion::Spring;
use crate::settings::{Settings, ThemeChoice};
use crate::theme::{
    SIDEBAR_W, TILE_GAP, TILE_RADIUS, TOPBAR_H, archive_theme, ease_out_cubic, ease_out_quint,
    fade, hover_overlay,
};
use crate::thumbs::{self, ThumbCache};
use crate::viewer::{
    ANIM_EXTENSIONS, ANIM_GPU_BUDGET, ANIM_MAX_SIDE, BarLayout, CONTROLS_FADE, CONTROLS_IDLE,
    FULL_DECODE_TIMEOUT, KEY_ZOOM_STEP, MediaKind, MediaView, Playback, SEEK_STEP_MS, VOLUME_HOLD,
    VOLUME_STEP, ViewTransform, Viewer, WHEEL_ZOOM, ZoomStep, actual_zoom, bar_layout, clamp_zoom,
    controls_wanted, ease_view, fraction_along, image_rect, is_double_click, max_zoom, pan,
    zoom_at,
};
use crate::worker::{self, AppEvent, TexKind, TextureJob, TexturePool, VaultDone, VaultKind};
use archive_core::{
    Album, AudioMeta, COVER_LAYERS, Collection, ImportEvent, ImportReport, Item, Kind, Library,
    LockState, LyricLine, MIN_PASSWORD_CHARS, OptimizeConfig, PageCursor, Stats, ThumbState,
    TimelineFilter, album_key, albums, current_lyric, human_bytes, parse_lyrics,
};
use dream::DEFAULT_SCENE_WGSL;
use dream::gfx::Renderer;
use dream::gfx::vulkan::GpuContext;
use dream::settings::{AaMode, Settings as RenderSettings, UpscalePreset};
use dream::texture::GpuTexture;
use dream::ui::draw::UiFrame;
use dream::ui::font::Align;
use dream::ui::theme::Theme;
use dream::ui::tokens::{Elevation, FontWeight};
use dream::ui::widgets::{LabelStyle, Ui, UiInput};
use dream::ui::{Color, Rect};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use winit::event_loop::EventLoopProxy;
use winit::window::Window;
use zeroize::Zeroize;

/// Thumbnails the grid keeps on the GPU at least. A session that shows more
/// tiles grows the cache to fit them, see `draw_grid`.
const THUMB_CACHE: usize = 180;
/// Time spent uploading finished textures in one frame. Each upload waits on
/// the GPU, so the queue is drained a few at a time rather than all at once.
const TEXTURE_BUDGET: Duration = Duration::from_millis(4);
/// How far the scroll must move before queued thumbnails for tiles that have
/// left the window are dropped.
const PRUNE_STEP: f32 = 32.0;

/// A decoded texture waiting for its upload. It is uploaded only while its
/// epoch is still the current vault epoch, see `TextureJob::epoch`.
struct ReadyTexture {
    key: String,
    id: String,
    kind: TexKind,
    epoch: u64,
    result: Result<(u32, u32, Vec<u8>), String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Media,
    /// Albums made from the album tags of imported music. Sorted by the
    /// library itself, so they cannot be made, renamed or deleted here.
    Albums,
    /// One of those albums, by its `album_key`.
    Album(String),
    /// The user's own collections, which they create and arrange.
    Collections,
    Collection(i64),
    Trash,
}

impl Page {
    pub fn is_media(&self) -> bool {
        matches!(
            self,
            Page::Media | Page::Album(_) | Page::Collection(_) | Page::Trash
        )
    }
}

/// A button on the top-right of the bar. Pages that list the same buttons in
/// the same order share them outright; a page switch rolls them otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopAction {
    Import,
    DeleteCollection(i64),
    /// Locks, or unlocks, the collection on screen: whichever it is not now.
    Vault(i64),
    EmptyTrash,
    Search,
}

/// The message a page shows while it has nothing to draw. Switching between
/// them is animated, so each has to be identifiable on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmptyKind {
    Searching,
    NoMatches,
    Trash,
    Collection,
    /// A locked collection, whose contents are not shown until it is unlocked.
    Locked,
    Albums,
    Media,
}

/// Idle time after which every unlocked collection locks itself. Input inside
/// the window resets it; looking at a picture inside a collection holds it off.
const VAULT_IDLE: Duration = Duration::from_secs(60);
/// One caret blink cycle of the search field, in seconds, and how long of it
/// the caret is shown.
const CARET_PERIOD: f32 = 1.06;
const CARET_ON: f32 = 0.58;

/// Stiffness of the chrome springs. Critically damped, so the island and the
/// header land together in about a fifth of a second.
const CHROME_OMEGA: f32 = 24.0;
/// The Details panel moves on the same spring, a little brisker than the chrome.
const DETAILS_OMEGA: f32 = 28.0;
/// Rows read per timeline page. A normal window fills from one page, and
/// opening a hundred thousand photos costs the same as opening four hundred.
const TIMELINE_PAGE: usize = 400;
/// Ceiling on the items a text or date search keeps in memory. The semantic
/// index already returns at most a few hundred.
const SEARCH_LIMIT: usize = 2000;

/// How long one empty-state message takes to dissolve into the next. Short
/// enough to feel immediate, long enough that the swap never pops.
const EMPTY_FADE: f32 = 0.16;
/// A search has to be slow enough to notice before it earns a "Searching…"
/// message; quick ones resolve straight to their result, with no flash.
const SEARCH_NOTICE_DELAY: f32 = 0.3;
/// Speed of a Details block dropping open or closed.
const DROP_OMEGA: f32 = 30.0;
/// Rate the full-view opener eases in at, and the brisker rate it leaves at:
/// a dismissal should feel finished by the time the eye has moved on.
const VIEWER_OPEN_RATE: f32 = 22.0;
const VIEWER_CLOSE_RATE: f32 = 44.0;
const DETAILS_INNER_W: f32 = 304.0;
const DETAILS_BODY_W: f32 = 280.0;
/// Height of a block's summary row, and the gap between blocks.
const CARD_HEAD_H: f32 = 52.0;
const CARD_GAP: f32 = 8.0;

/// Space a block's body takes when fully open: its fields plus padding.
fn drop_height(body: f32) -> f32 {
    if body > 0.0 { body + 18.0 } else { 0.0 }
}
/// The search field opens on its own spring, quick enough to feel immediate.
const SEARCH_OMEGA: f32 = 26.0;
/// The new-collection field shuts on a stiffer spring. Opening from rest is
/// gentle, but a close that eases in from rest drags, so it leaves brisker.
const NAME_SHUT_OMEGA: f32 = 44.0;
/// Header names for the three island rows, top to bottom.
const NAV_TITLES: [&str; 4] = ["Media", "Albums", "Collections", "Trash"];
const TITLE_SIZE: f32 = 24.0;
/// Vertical distance between neighbouring names on the header drum.
const TITLE_STEP: f32 = 36.0;
/// How many rows the island may travel off an open page before the name's own
/// roll stops, so it fades out in step with the drum rather than popping.
const NAME_ROLL_REACH: f32 = 0.25;
/// Edge of a round top-right button, and of the search field at rest.
const ACTION_BOX: f32 = 40.0;
/// Edge of an open album's cover, at the top of its page.
const ALBUM_COVER: f32 = 200.0;
/// Height of one song on an open album's page.
const SONG_ROW_H: f32 = 52.0;
/// Widest an open album's song list runs, so rows do not stretch across a wide window.
const SONG_LIST_MAX_W: f32 = 720.0;

/// Context menu geometry. The card is the modal card's surface, with rows
/// nested inside it: row radius is the card radius less the padding.
const MENU_MIN_W: f32 = 200.0;
const MENU_MAX_W: f32 = 300.0;
const MENU_PAD: f32 = 6.0;
const MENU_RADIUS: f32 = 12.0;
const MENU_ROW_RADIUS: f32 = 8.0;
const MENU_ROW_H: f32 = 34.0;
const MENU_HEADER_H: f32 = 28.0;
/// Where a row's icon and label start, from the card's left edge.
const MENU_ICON_X: f32 = 14.0;
const MENU_LABEL_X: f32 = 42.0;
/// Distance kept from the window edges when the menu is placed.
const MENU_MARGIN: f32 = 8.0;
/// A context menu pops up at once and fades out over this many seconds.
const MENU_FADE_SECS: f32 = 0.12;

/// The island row a page lights up. An open album keeps Albums lit, and an
/// open collection keeps Collections lit.
fn nav_row(page: &Page) -> f32 {
    match page {
        Page::Media => 0.0,
        Page::Albums | Page::Album(_) => 1.0,
        Page::Collections | Page::Collection(_) => 2.0,
        Page::Trash => 3.0,
    }
}

/// Vertical offset of an open page's name on the header drum. The name always
/// rides the drum, so when the island turns away it rolls out with the rows.
/// Only while the island still sits on the open row does the name add its own
/// roll-in (opening) or roll-out (closing) on top, and that extra fades away as
/// the island moves off, so the name never drifts from its slot.
fn name_roll(open_row: f32, nav: f32, open_t: f32) -> f32 {
    let on_row = (1.0 - (nav - open_row).abs() / NAME_ROLL_REACH).clamp(0.0, 1.0);
    (open_row - nav) * TITLE_STEP + (1.0 - open_t) * TITLE_STEP * on_row
}

/// Where each top-right button sits at rest, right to left. Search comes last
/// and is the only one whose width follows its open state.
fn top_action_rects(
    actions: &[TopAction],
    content_x: f32,
    content_right: f32,
    search_t: f32,
) -> Vec<(TopAction, Rect)> {
    let mut right = content_right - 20.0;
    let mut slots = Vec::with_capacity(actions.len());
    for &action in actions {
        match action {
            TopAction::Search => {
                let open_w = (right - content_x - 216.0).max(0.0).clamp(180.0, 340.0);
                let w = ACTION_BOX + (open_w - ACTION_BOX) * search_t;
                slots.push((action, Rect::new(right - w, 13.0, w, ACTION_BOX)));
            }
            _ => {
                slots.push((action, Rect::new(right - 40.0, 13.0, 40.0, 40.0)));
                right -= 44.0;
            }
        }
    }
    slots
}

/// The search field is open while it holds text, the pointer is on it, or it
/// is selected. Everything else (including the spring) reads this.
fn search_is_open(empty: bool, hovered: bool, focused: bool) -> f32 {
    if empty && !hovered && !focused {
        0.0
    } else {
        1.0
    }
}

fn open_amount(page: &Page) -> f32 {
    if matches!(page, Page::Album(_) | Page::Collection(_)) {
        1.0
    } else {
        0.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct Input {
    pub mouse: (f32, f32),
    pub down: bool,
    pub pressed: bool,
    pub released: bool,
    /// The right button went down this frame. It opens a context menu.
    pub secondary_pressed: bool,
    pub wheel: f32,
    pub precise: f32,
    pub ctrl: bool,
    pub shift: bool,
}

#[derive(Clone, Debug)]
struct Press {
    id: String,
    index: usize,
    at: (f32, f32),
}

#[derive(Clone, Debug)]
enum ConfirmAction {
    TrashIds(Vec<String>),
    DeleteForever(Vec<String>),
    EmptyTrash,
    DeleteCollection(i64),
    /// Take the lock off an unlocked collection.
    RemoveLock(i64),
}

#[derive(Clone, Debug)]
enum Modal {
    ImportSource,
    RemoteImport(Arc<crate::remote_import::Session>),
    RemoteImportDetails(Arc<crate::remote_import::Session>),
    Confirm {
        title: String,
        body: String,
        confirm: String,
        action: ConfirmAction,
    },
    AddToCollection {
        ids: Vec<String>,
        name: String,
        /// The search line's text: narrows the collections the sheet shows.
        query: String,
        focus: SheetField,
        /// Whether the new-collection name field is showing. It stays hidden
        /// until the plus is pressed, unless there is nothing to pick from.
        creating: bool,
        /// Collections that already hold every selected item, and so have
        /// nothing to gain from this add.
        skip: HashSet<i64>,
    },
    /// An import, from the first copied file to the decision to keep it.
    Import(ImportSession),
    /// Setting, entering or removing a collection's password.
    Password(PasswordSession),
}

/// What a password dialog is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasswordMode {
    /// Lock an ordinary collection. The password is typed twice.
    Create,
    /// Unlock a locked collection to see inside it.
    Unlock,
    /// Unlock a locked collection, then take its lock away.
    RemoveLock,
}

/// The field of a password dialog that takes typed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasswordField {
    Password,
    Confirm,
}

/// A password dialog on screen. The typed passwords are wiped when the dialog
/// goes, whether it was confirmed, cancelled or dismissed, so they do not stay
/// in memory any longer than the dialog needs them.
#[derive(Clone)]
struct PasswordSession {
    collection: i64,
    mode: PasswordMode,
    password: String,
    confirm: String,
    field: PasswordField,
    /// Why the last attempt failed, shown under the fields.
    error: Option<String>,
    /// True while the lock is being worked on in the background.
    busy: bool,
}

impl std::fmt::Debug for PasswordSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the typed passwords, even in a debug trace.
        f.debug_struct("PasswordSession")
            .field("collection", &self.collection)
            .field("mode", &self.mode)
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

impl Drop for PasswordSession {
    fn drop(&mut self) {
        self.password.zeroize();
        self.confirm.zeroize();
    }
}

impl PasswordSession {
    /// The text of the focused field, which is where typing goes.
    fn typed_mut(&mut self) -> &mut String {
        match self.field {
            PasswordField::Password => &mut self.password,
            PasswordField::Confirm => &mut self.confirm,
        }
    }
}

/// One import on its sheet. Its rows are committed as they are copied, but
/// they only count once Confirm is pressed: Cancel takes them back.
#[derive(Clone, Debug)]
struct ImportSession {
    phase: ImportPhase,
    total: usize,
    done: usize,
    current: String,
    /// Raised by Cancel. The copy stops starting new files once it sees it.
    cancel: Arc<AtomicBool>,
    /// What the import did, once the copy has finished.
    report: Option<ImportReport>,
    /// Why taking the import back failed. The sheet stays open to try again.
    error: Option<String>,
    /// Files dropped while this sheet was open. They start once it closes.
    queued: Vec<PathBuf>,
    /// When the import finished without problems. The sheet then reads
    /// "Import complete" and closes itself after a moment.
    finished_at: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportPhase {
    /// Files are being copied.
    Running,
    /// Cancel was pressed mid-copy and the copy is winding down.
    Stopping,
    /// Everything is processed and something failed. Confirm keeps the import;
    /// Cancel removes it.
    Review,
    /// Everything is processed and nothing failed. Shown briefly, then kept.
    Complete,
    /// The import's rows are being removed.
    Removing,
}

/// How a modal button reads: quiet, the main action, or a destructive one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModalTone {
    Plain,
    /// The main action: filled in the foreground colour, as the sheet's plus.
    Primary,
    Danger,
}

/// The field of the collection sheet that takes typed text: the search line
/// that narrows the collections, or the name of a new one. `Idle` means
/// neither has the focus, because a press went somewhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SheetField {
    Search,
    Name,
    Idle,
}

/// The collection sheet's metrics: a header and a search line, a grid of
/// collection tiles that scrolls, and a footer for the name field.
const SHEET_MAX_W: f32 = 920.0;
/// The gap between the sheet and the window's sides.
const SHEET_MARGIN: f32 = 24.0;
/// The padding between the sheet's edges and its content.
const SHEET_PAD: f32 = 24.0;
const SHEET_HEADER_H: f32 = 60.0;
const SHEET_FIELD_H: f32 = 38.0;
const SHEET_GAP: f32 = 14.0;
/// Tall enough for a preview, with the name and count over its lower edge.
const SHEET_TILE_H: f32 = 112.0;
const SHEET_TILE_MIN_W: f32 = 200.0;
const SHEET_TILE_GAP: f32 = 10.0;
/// How much of the panel's colour sits over a tile's cover, 0 to 255: enough
/// to quiet the picture without hiding it.
const SHEET_TILE_TINT: u8 = 150;
/// How far each sheet of a cover stack sits from the one in front, as a share
/// of the cover's width.
const COVER_STEP: f32 = 0.05;
/// The import sheet is narrower than the collection sheet: it has one job.
const IMPORT_SHEET_W: f32 = 520.0;
/// How long a clean import shows "Import complete" before the sheet eases out.
/// The easing is the same one the sheet enters with.
const IMPORT_COMPLETE_HOLD: f32 = 1.2;
/// Where the import sheet's body starts, below its header.
const IMPORT_BODY_TOP: f32 = 74.0;
const IMPORT_ROW_H: f32 = 18.0;
/// Problems named by file. The rest are counted on one line.
const IMPORT_PROBLEMS_SHOWN: usize = 3;

/// How the collection sheet is laid out.
struct SheetLayout {
    width: f32,
    height: f32,
    /// Tiles across the grid.
    columns: usize,
    /// The header and the search line, above the grid.
    head: f32,
    /// The grid's visible part, between `head` and the footer.
    grid: f32,
    /// How far the grid can scroll.
    max_scroll: f32,
}

/// The sheet is as wide as the window allows, up to `SHEET_MAX_W`, and as
/// tall as its content, up to 80% of the window. Its columns follow its
/// width. It is sized from every collection offered, not from the search, so
/// it holds still while the search narrows the list.
fn collection_sheet_layout(
    window_w: f32,
    window_h: f32,
    total: usize,
    offered: usize,
) -> SheetLayout {
    let width = (window_w - 2.0 * SHEET_MARGIN).clamp(0.0, SHEET_MAX_W);
    let inner_w = (width - 2.0 * SHEET_PAD).max(0.0);
    let columns = ((inner_w + SHEET_TILE_GAP) / (SHEET_TILE_MIN_W + SHEET_TILE_GAP))
        .floor()
        .clamp(1.0, 4.0) as usize;
    let grid_content = if offered == 0 {
        // With collections that already hold the selection, one line says so.
        if total > 0 { SHEET_TILE_H } else { 0.0 }
    } else {
        let rows = offered.div_ceil(columns);
        rows as f32 * SHEET_TILE_H + (rows - 1) as f32 * SHEET_TILE_GAP
    };
    // The search lives in the header, so the grid only needs a gap under it.
    let head = SHEET_HEADER_H + if total > 0 { SHEET_GAP } else { 0.0 };
    // The footer is one row: the name field while creating, otherwise the
    // plus that opens it. The gap above it only applies beneath a grid.
    let footer = (if total > 0 { SHEET_GAP } else { 0.0 }) + SHEET_FIELD_H + 22.0;
    let fit = (window_h * 0.8).max(head + footer + SHEET_TILE_H);
    let height = (head + grid_content + footer).min(fit);
    let grid = (height - head - footer).max(0.0);
    SheetLayout {
        width,
        height,
        columns,
        head,
        grid,
        max_scroll: (grid_content - grid).max(0.0),
    }
}

/// The sheet's height: its header, the body for its phase, and the button row
/// where the phase has one.
fn import_sheet_height(session: &ImportSession) -> f32 {
    let (body, footer) = match session.phase {
        // The counts and the bar.
        ImportPhase::Running | ImportPhase::Stopping => (32.0, 18.0 + SHEET_FIELD_H),
        ImportPhase::Complete => {
            let detail = session
                .report
                .as_ref()
                .is_some_and(|report| !import_detail(report).is_empty());
            (20.0 + if detail { 24.0 } else { 0.0 }, 0.0)
        }
        ImportPhase::Review | ImportPhase::Removing => {
            let mut body = 42.0;
            if let Some(report) = &session.report
                && !report.failures.is_empty()
            {
                body += 26.0 + problem_rows(report) as f32 * IMPORT_ROW_H;
            }
            if session.error.is_some() {
                body += 26.0;
            }
            (body, 18.0 + SHEET_FIELD_H)
        }
    };
    IMPORT_BODY_TOP + body + footer + 22.0
}

/// Lines the problem list takes: the first problems by name, then one line
/// counting the rest.
fn problem_rows(report: &ImportReport) -> usize {
    let count = report.failures.len();
    count.min(IMPORT_PROBLEMS_SHOWN) + usize::from(count > IMPORT_PROBLEMS_SHOWN)
}

/// The headline of a finished import.
fn import_summary(report: &ImportReport) -> String {
    if report.imported > 0 {
        format!(
            "Added {} {} · {}",
            report.imported,
            if report.imported == 1 {
                "item"
            } else {
                "items"
            },
            human_bytes(report.bytes_imported)
        )
    } else if report.duplicates > 0 {
        "Everything was already in your archive".into()
    } else if report.failed > 0 {
        "Some files could not be imported".into()
    } else {
        "No photos, videos or audio were found".into()
    }
}

/// The second line of a finished import: what was already there, and what
/// was passed over. Empty when there is nothing to add.
fn import_detail(report: &ImportReport) -> String {
    let mut parts = Vec::new();
    if report.imported > 0 && report.duplicates > 0 {
        parts.push(format!("{} already in your archive", report.duplicates));
    }
    if report.skipped > 0 {
        parts.push(format!("{} skipped", report.skipped));
    }
    parts.join(" · ")
}

/// A file's name, for a line of progress or a problem.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// One tile on the Albums or Collections page: a cover stack, a name and a
/// count. Both pages draw the same tile, so only what it links to differs.
#[derive(Clone, Debug)]
struct CoverTile {
    /// Keys the tile's cover images in `covers`.
    cover_key: String,
    name: String,
    count_label: String,
    page: Page,
    /// A locked collection shows a lock in place of its cover.
    lock: LockState,
}

/// The Media page as it was when an album was opened from it: how far it had
/// scrolled, and how many rows were loaded, so the return lands on the same spot.
#[derive(Clone, Copy, Debug)]
struct MediaResume {
    scroll: f32,
    loaded: usize,
}

/// "1 song", "3 songs": the count under a tile.
fn count_label(count: i64, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// The collections the sheet offers: those that do not already hold the whole
/// selection and whose name contains `query`, ignoring case.
fn sheet_offers(collections: &[Collection], skip: &HashSet<i64>, query: &str) -> Vec<Collection> {
    let query = query.trim().to_lowercase();
    collections
        .iter()
        .filter(|collection| {
            !skip.contains(&collection.id)
                && collection.lock != LockState::Locked
                && collection.name.to_lowercase().contains(&query)
        })
        .cloned()
        .collect()
}

#[derive(Clone, Debug)]
struct Toast {
    text: String,
    age: f32,
}

/// What a context menu row does once it is chosen. Each one carries the ids
/// it acts on, taken when the menu opened, so the action cannot drift from
/// what the menu showed.
#[derive(Clone, Debug, PartialEq)]
enum MenuAction {
    ToggleDetails,
    AddToCollection(Vec<String>),
    RemoveFromAlbum(Vec<String>),
    Restore(Vec<String>),
    Trash(Vec<String>),
    DeleteForever(Vec<String>),
    DeleteCollection(i64),
    /// Ask for a password to lock an ordinary collection.
    StartLock(i64),
    /// Ask for a password to unlock a locked collection.
    StartUnlock(i64),
    /// Take a collection's lock away. Asks for the password unless it is
    /// already unlocked.
    StartRemoveLock(i64),
    /// Seal an unlocked collection now.
    LockNow(i64),
}

#[derive(Clone, Debug)]
enum MenuRow {
    /// Says how many items the menu acts on, above the rows.
    Header(String),
    Item {
        label: String,
        icon: AppIcon,
        danger: bool,
        action: MenuAction,
    },
}

impl MenuRow {
    fn item(label: &str, icon: AppIcon, danger: bool, action: MenuAction) -> Self {
        Self::Item {
            label: label.to_string(),
            icon,
            danger,
            action,
        }
    }
}

/// A context menu, opened at the pointer. It shuts by fading out, so
/// `closing` stays set until its spring has run down.
#[derive(Clone, Debug)]
struct ContextMenu {
    at: (f32, f32),
    rows: Vec<MenuRow>,
    /// The row under the pointer or the keyboard, if any.
    focus: Option<usize>,
    closing: bool,
}

pub struct ArchiveApp {
    pub renderer: Option<Renderer>,
    pub context: Option<Arc<dream::gfx::vulkan::GpuContext>>,
    ui: Option<Ui>,
    pub theme: Theme,
    pub dark: bool,
    system_dark: bool,
    pub settings: Settings,
    pub library: Arc<Library>,

    pub stats: Stats,
    /// The user's collections, which the Collections page shows.
    pub collections: Vec<Collection>,
    /// Albums made from the music's album tags, which the Albums page shows.
    pub albums: Vec<Album>,
    /// The tiles the Albums or Collections page shows: its collections, narrowed by
    /// the current search. A collection matches by name or through any image in
    /// it; a music album matches by name.
    cover_tiles: Vec<CoverTile>,
    /// Each album's cover images, newest first, keyed by the tile's cover key.
    covers: HashMap<String, Vec<Item>>,
    pub items: Vec<Item>,
    pub display: Vec<Item>,
    /// Rows in the current filter, including pages not loaded yet.
    timeline_total: usize,
    /// Where the loaded prefix stops; `None` when nothing is loaded.
    timeline_cursor: Option<PageCursor>,
    /// Tags of the music tracks seen so far, by item id. The album pages and the
    /// details panel read them; they are small and never change once imported.
    track_tags: HashMap<String, AudioMeta>,
    /// True once every row of the filter is in `items`.
    timeline_done: bool,
    /// Items for the active text or date search, fetched by id so a search
    /// reaches the whole library instead of only the loaded page.
    search_items: Vec<Item>,
    /// `(id, captured_at)` for the filter, read on demand for date searches.
    capture_index: Option<Vec<(String, i64)>>,
    pub page: Page,
    /// Where the Media page was when an album was opened from it, so the
    /// album's back arrow can return there. `None` for every other way in.
    media_resume: Option<MediaResume>,
    select_mode: bool,
    selection: BTreeSet<String>,
    last_selected: Option<usize>,

    pub scroll: f32,
    pub scroll_target: f32,
    pub scroll_max: f32,
    scroll_drag: Option<(f32, f32)>,
    grid: GridLayout,
    grid_key: (u32, usize),

    pub search: String,
    search_focus: bool,
    search_blink: f32,
    /// 0 is the bare search icon, 1 the fully open field.
    search_open: Spring,
    search_hover: bool,
    search_rect: Rect,
    search_debounce: f32,
    /// Seconds the current search has been outstanding, for the notice delay.
    search_wait: f32,
    search_pending: bool,
    search_seq: u64,
    search_inflight: Option<(u64, String)>,
    search_hits: Option<(String, Vec<String>)>,
    semantic: Option<crate::semantic::Semantic>,
    press: Option<Press>,

    hover: Option<String>,
    hover_anim: HashMap<String, f32>,
    /// How visible a tile's selection dot is: rides the same curve as the
    /// hover tint, but stays up while the tile is selected.
    check_anim: HashMap<String, f32>,
    /// The empty-state message on show and the one it replaced, plus the
    /// progress of the dissolve between them; `empty_shown` says whether it
    /// was on screen last frame.
    empty_state: Option<EmptyKind>,
    empty_prev: Option<EmptyKind>,
    empty_t: f32,
    empty_shown: bool,
    empty_drawn: bool,
    viewer: Option<Viewer>,
    /// Textures the viewer let go of. They wait out the frames still in flight
    /// before the GPU is allowed to free them.
    viewer_retired: Vec<(Arc<GpuTexture>, u8)>,
    /// The one full-resolution decode in flight: item id and when it was
    /// asked for. A decode that never answers (a worker that died) is given
    /// up after [`FULL_DECODE_TIMEOUT`] so later stills are not blocked.
    full_pending: Option<(String, Instant)>,
    /// Volume and mute carry from one video to the next for the session.
    video_volume: f32,
    video_muted: bool,
    /// Items whose full decode failed, so they are not retried every frame.
    full_failed: HashSet<String>,
    modal: Option<Modal>,
    modal_t: f32,
    /// When the app last had input inside its window. Idle time, for auto-lock,
    /// runs from here.
    last_activity: Instant,
    /// A lock, unlock or seal is running on a background thread. Vault actions
    /// wait for it, so two never race over the same collection.
    vault_busy: bool,
    /// The thread running the current vault job. Joined on exit, so a seal
    /// in progress finishes before the process ends.
    vault_thread: Option<std::thread::JoinHandle<()>>,
    /// How far the collection sheet's list is scrolled down.
    sheet_scroll: f32,
    /// Where the sheet's text fields and its plus sat last frame, so a press
    /// can tell whether it landed on them. Empty when not drawn.
    sheet_search_rect: Rect,
    sheet_name_rect: Rect,
    sheet_plus_rect: Rect,
    sheet_cancel_rect: Rect,
    sheet_rect: Rect,
    /// 0 with the sheet's search at rest, 1 with it open, moving as the Media
    /// search does.
    sheet_search: Spring,
    /// 0 with the new-collection plus at its left-hand rest, 1 once it has
    /// slid over beside the name field.
    sheet_create: Spring,
    toast: Option<Toast>,
    /// The context menu, while one is up or fading out.
    menu: Option<ContextMenu>,
    /// How far the menu has opened: 0 hidden, 1 fully shown.
    menu_t: f32,
    /// Where the menu's card sat last frame, for telling a press on it apart
    /// from a press that dismisses it.
    menu_rect: Rect,

    thumbs: ThumbCache,
    previews: ThumbCache,
    texture_pool: Option<TexturePool>,
    /// Bumped whenever a collection becomes locked. A texture asked for under an
    /// older epoch may still arrive, and it is dropped instead of shown.
    vault_epoch: u64,
    /// Finished viewer pictures, waiting for an upload slot. They go first.
    viewer_textures: VecDeque<ReadyTexture>,
    /// Finished grid thumbnails, waiting for an upload slot.
    grid_textures: VecDeque<ReadyTexture>,
    /// The scroll position the thumbnail queue was last pruned at.
    pruned_at: f32,
    thumb_pending: HashSet<String>,
    preview_pending: HashSet<String>,
    preview_started: HashMap<String, Instant>,
    thumb_failed: HashSet<String>,
    /// Failed preview attempts per item, with the time of the last try.
    preview_failed: HashMap<String, (u8, Instant)>,
    rebuilt: HashSet<String>,

    pub screen: (f32, f32),
    pub scale: f32,
    pub input: Input,
    pub dirty: bool,
    last_frame: Instant,
    /// Seconds since the previous frame, unclamped (see `render`).
    frame_real_dt: f32,
    time: f32,
    frame_dt: f32,
    animating: bool,
    /// Position of the island highlight, in rows: 0 is Media, 2 is Trash.
    nav: Spring,
    /// Visibility of the Trash retention subheader, on the same spring as the
    /// page name revolver.
    trash_notice: Spring,
    /// 1 while an album or collection is open, 0 otherwise: drives the header swap.
    opened: Spring,
    /// Last open album's name, kept so it can roll out after closing.
    open_title: String,
    /// The nav row the last open album sits under, so its name rolls in and
    /// out on that row. Albums and Collections have different rows.
    open_row: usize,
    /// Which Details blocks are open, and their drop springs. Kept across items.
    details_open: [bool; details::BLOCK_COUNT],
    details_spring: [Spring; details::BLOCK_COUNT],
    details_scroll: f32,

    events_tx: Sender<AppEvent>,
    events_rx: Receiver<AppEvent>,
    proxy: Option<EventLoopProxy<()>>,
    pub cursor_hand: bool,
    pub last_error: Option<String>,
}

impl ArchiveApp {
    pub fn new(window: Arc<Window>, proxy: EventLoopProxy<()>) -> Result<Self, String> {
        let settings = Settings::load();
        let system_dark = window.theme() == Some(winit::window::Theme::Dark);
        let library =
            Arc::new(Library::open(settings.library_root()).map_err(|error| error.to_string())?);
        let dark = match settings.theme {
            ThemeChoice::System => system_dark,
            ThemeChoice::Light => false,
            ThemeChoice::Dark => true,
        };
        let theme = archive_theme(dark);
        let mut ui = Ui::new(
            crate::fonts::roboto(),
            dream::ui::atlas::AtlasConfig::default(),
        );
        ui.set_theme(&theme);
        let renderer = Renderer::new(
            Arc::clone(&window),
            DEFAULT_SCENE_WGSL,
            RenderSettings {
                aa: AaMode::None,
                upscale: UpscalePreset::Native,
                sharpness: 0.6,
            },
        );
        let context = Some(Arc::clone(renderer.context()));
        let scale = window.scale_factor() as f32;
        let physical = window.inner_size();
        let screen = (
            physical.width as f32 / scale.max(1.0),
            physical.height as f32 / scale.max(1.0),
        );
        let (events_tx, events_rx) = mpsc::channel();
        let texture_pool = Some(worker::texture_pool(
            Arc::clone(&library),
            events_tx.clone(),
            proxy.clone(),
            4,
        ));
        let mut app = Self {
            renderer: Some(renderer),
            context,
            ui: Some(ui),
            theme,
            dark,
            system_dark,
            settings,
            library,
            stats: Stats::default(),
            collections: Vec::new(),
            albums: Vec::new(),
            cover_tiles: Vec::new(),
            covers: HashMap::new(),
            items: Vec::new(),
            display: Vec::new(),
            timeline_total: 0,
            timeline_cursor: None,
            track_tags: HashMap::new(),
            timeline_done: true,
            search_items: Vec::new(),
            capture_index: None,
            page: Page::Media,
            media_resume: None,
            select_mode: false,
            selection: BTreeSet::new(),
            last_selected: None,
            scroll: 0.0,
            scroll_target: 0.0,
            scroll_max: 0.0,
            scroll_drag: None,
            grid: GridLayout::default(),
            grid_key: (0, 0),
            search: String::new(),
            search_focus: false,
            search_blink: 0.0,
            search_open: Spring::at(0.0),
            search_hover: false,
            search_rect: Rect::default(),
            search_debounce: 0.0,
            search_wait: 0.0,
            search_pending: false,
            search_seq: 0,
            search_inflight: None,
            search_hits: None,
            semantic: None,
            press: None,
            hover: None,
            hover_anim: HashMap::new(),
            check_anim: HashMap::new(),
            empty_state: None,
            empty_prev: None,
            empty_t: 1.0,
            empty_shown: false,
            empty_drawn: false,
            viewer: None,
            viewer_retired: Vec::new(),
            full_pending: None,
            video_volume: 1.0,
            video_muted: false,
            full_failed: HashSet::new(),
            modal: None,
            modal_t: 0.0,
            last_activity: Instant::now(),
            vault_busy: false,
            vault_thread: None,
            sheet_scroll: 0.0,
            sheet_search_rect: Rect::default(),
            sheet_name_rect: Rect::default(),
            sheet_plus_rect: Rect::default(),
            sheet_cancel_rect: Rect::default(),
            sheet_rect: Rect::default(),
            sheet_search: Spring::at(0.0),
            sheet_create: Spring::at(0.0),
            toast: None,
            menu: None,
            menu_t: 0.0,
            menu_rect: Rect::default(),
            thumbs: ThumbCache::new(THUMB_CACHE),
            previews: ThumbCache::new(3),
            texture_pool,
            vault_epoch: 0,
            viewer_textures: VecDeque::new(),
            grid_textures: VecDeque::new(),
            pruned_at: f32::NEG_INFINITY,
            thumb_pending: HashSet::new(),
            preview_pending: HashSet::new(),
            preview_started: HashMap::new(),
            thumb_failed: HashSet::new(),
            preview_failed: HashMap::new(),
            rebuilt: HashSet::new(),
            screen,
            scale,
            input: Input::default(),
            dirty: true,
            last_frame: Instant::now(),
            frame_real_dt: 0.0,
            time: 0.0,
            frame_dt: 0.0,
            animating: false,
            nav: Spring::at(0.0),
            trash_notice: Spring::at(0.0),
            opened: Spring::at(0.0),
            open_title: String::new(),
            open_row: 2,
            details_open: [false; details::BLOCK_COUNT],
            details_spring: [Spring::at(0.0); details::BLOCK_COUNT],
            details_scroll: 0.0,
            events_tx,
            events_rx,
            proxy: Some(proxy),
            cursor_hand: false,
            last_error: None,
        };
        app.reload();
        app.observe_theme();
        app.semantic = crate::semantic::Semantic::start(
            Arc::clone(&app.library),
            app.events_tx.clone(),
            app.proxy.clone(),
        );
        Ok(app)
    }

    /// A renderer-free instance used by layout tests and the selfcheck's
    /// first phase. No worker threads are started.
    pub fn headless(root: PathBuf, settings: Settings) -> Result<Self, String> {
        let library = Arc::new(Library::open(root).map_err(|e| e.to_string())?);
        let (events_tx, events_rx) = mpsc::channel();
        let theme = archive_theme(false);
        let mut ui = Ui::new(
            crate::fonts::roboto(),
            dream::ui::atlas::AtlasConfig::default(),
        );
        ui.set_theme(&theme);
        let mut app = Self {
            renderer: None,
            context: None,
            ui: Some(ui),
            theme,
            dark: false,
            system_dark: false,
            settings,
            library,
            stats: Stats::default(),
            collections: Vec::new(),
            albums: Vec::new(),
            cover_tiles: Vec::new(),
            covers: HashMap::new(),
            items: Vec::new(),
            display: Vec::new(),
            timeline_total: 0,
            timeline_cursor: None,
            track_tags: HashMap::new(),
            timeline_done: true,
            search_items: Vec::new(),
            capture_index: None,
            page: Page::Media,
            media_resume: None,
            select_mode: false,
            selection: BTreeSet::new(),
            last_selected: None,
            scroll: 0.0,
            scroll_target: 0.0,
            scroll_max: 0.0,
            scroll_drag: None,
            grid: GridLayout::default(),
            grid_key: (0, 0),
            search: String::new(),
            search_focus: false,
            search_blink: 0.0,
            search_open: Spring::at(0.0),
            search_hover: false,
            search_rect: Rect::default(),
            search_debounce: 0.0,
            search_wait: 0.0,
            search_pending: false,
            search_seq: 0,
            search_inflight: None,
            search_hits: None,
            semantic: None,
            press: None,
            hover: None,
            hover_anim: HashMap::new(),
            check_anim: HashMap::new(),
            empty_state: None,
            empty_prev: None,
            empty_t: 1.0,
            empty_shown: false,
            empty_drawn: false,
            viewer: None,
            viewer_retired: Vec::new(),
            full_pending: None,
            video_volume: 1.0,
            video_muted: false,
            full_failed: HashSet::new(),
            modal: None,
            modal_t: 0.0,
            last_activity: Instant::now(),
            vault_busy: false,
            vault_thread: None,
            sheet_scroll: 0.0,
            sheet_search_rect: Rect::default(),
            sheet_name_rect: Rect::default(),
            sheet_plus_rect: Rect::default(),
            sheet_cancel_rect: Rect::default(),
            sheet_rect: Rect::default(),
            sheet_search: Spring::at(0.0),
            sheet_create: Spring::at(0.0),
            toast: None,
            menu: None,
            menu_t: 0.0,
            menu_rect: Rect::default(),
            thumbs: ThumbCache::new(16),
            previews: ThumbCache::new(2),
            texture_pool: None,
            vault_epoch: 0,
            viewer_textures: VecDeque::new(),
            grid_textures: VecDeque::new(),
            pruned_at: f32::NEG_INFINITY,
            thumb_pending: HashSet::new(),
            preview_pending: HashSet::new(),
            preview_started: HashMap::new(),
            thumb_failed: HashSet::new(),
            preview_failed: HashMap::new(),
            rebuilt: HashSet::new(),
            screen: (1440.0, 900.0),
            scale: 1.0,
            input: Input::default(),
            dirty: true,
            last_frame: Instant::now(),
            frame_real_dt: 0.0,
            time: 0.0,
            frame_dt: 0.0,
            animating: false,
            nav: Spring::at(0.0),
            trash_notice: Spring::at(0.0),
            opened: Spring::at(0.0),
            open_title: String::new(),
            open_row: 2,
            details_open: [false; details::BLOCK_COUNT],
            details_spring: [Spring::at(0.0); details::BLOCK_COUNT],
            details_scroll: 0.0,
            events_tx,
            events_rx,
            proxy: None,
            cursor_hand: false,
            last_error: None,
        };
        app.reload();
        Ok(app)
    }

    // --- Host plumbing ---------------------------------------------------

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.max(0.5);
        if let Some(ui) = self.ui.as_mut() {
            ui.set_scale(self.scale);
        }
    }

    pub fn resize(&mut self, physical: (u32, u32)) {
        self.screen = (
            physical.0 as f32 / self.scale.max(1.0),
            physical.1 as f32 / self.scale.max(1.0),
        );
        self.dirty = true;
    }

    pub fn observe_theme(&mut self) {
        if self.settings.theme == ThemeChoice::System {
            self.apply_theme(self.system_dark);
        }
    }

    pub fn set_system_dark(&mut self, dark: bool) {
        self.system_dark = dark;
        self.observe_theme();
    }

    fn apply_theme(&mut self, dark: bool) {
        self.dark = dark;
        self.theme = archive_theme(dark);
        if let Some(ui) = self.ui.as_mut() {
            ui.set_theme(&self.theme);
        }
        self.thumbs.clear();
        self.dirty = true;
    }

    pub fn pointer_moved(&mut self, at: (f32, f32)) {
        self.input.mouse = at;
        // Moving the pointer brings the video bar up, and keeps it up a while.
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.controls_until = Instant::now() + CONTROLS_IDLE;
        }
        self.dirty = true;
    }

    pub fn pointer_button(&mut self, down: bool) {
        if down {
            self.input.down = true;
            self.input.pressed = true;
            self.blur_text_fields();
        } else {
            self.input.down = false;
            self.input.released = true;
        }
        self.dirty = true;
    }

    /// A right press opens the context menu for whatever is under the
    /// pointer; the page that is drawn decides what that is. A press on the
    /// open menu itself is the menu's own, and is not passed on.
    pub fn pointer_secondary(&mut self) {
        if self.menu.is_some() && self.menu_rect.contains(self.input.mouse) {
            return;
        }
        self.input.secondary_pressed = true;
        self.dirty = true;
    }

    /// A press anywhere outside a text field takes the focus off it. Leaving
    /// the new-collection name field cancels creating, so the field only
    /// stays while the press is on it or on its plus, which creates.
    fn blur_text_fields(&mut self) {
        let at = self.input.mouse;
        if self.search_focus && !self.search_rect.contains(at) {
            self.search_focus = false;
        }
        let on_search = self.sheet_search_rect.contains(at);
        let on_name = self.sheet_name_rect.contains(at) || self.sheet_plus_rect.contains(at);
        // A press that closes the sheet, on Cancel or off the sheet, must not
        // also collapse the name field: that would flash the plus as if it had
        // been re-enabled while the sheet is still on screen.
        let closes = self.sheet_cancel_rect.contains(at) || !self.sheet_rect.contains(at);
        if let Some(Modal::AddToCollection {
            focus, creating, ..
        }) = self.modal.as_mut()
        {
            match *focus {
                SheetField::Search if !on_search => *focus = SheetField::Idle,
                SheetField::Name if !on_name && !closes => {
                    *focus = SheetField::Idle;
                    *creating = false;
                }
                _ => {}
            }
        }
    }

    pub fn wheel(&mut self, pixels: f32, precise: f32) {
        self.input.wheel += pixels;
        self.input.precise += precise;
        self.dirty = true;
    }

    pub fn modifiers(&mut self, ctrl: bool, shift: bool) {
        self.input.ctrl = ctrl;
        self.input.shift = shift;
    }

    pub fn text_input(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return;
        }
        match &mut self.modal {
            Some(Modal::AddToCollection {
                name, query, focus, ..
            }) => match focus {
                SheetField::Name => name.push_str(&text),
                SheetField::Search => {
                    query.push_str(&text);
                    self.sheet_scroll = 0.0;
                }
                SheetField::Idle => {}
            },
            Some(Modal::Password(session)) => {
                if !session.busy {
                    session.typed_mut().push_str(&text);
                }
            }
            Some(_) => {}
            None => {
                if self.search_focus {
                    self.search.push_str(&text);
                    self.filter_display();
                }
            }
        }
        self.dirty = true;
    }

    pub fn key(&mut self, key: dream::ui::widgets::Key, pressed: bool) {
        use dream::ui::widgets::Key;
        match key {
            Key::Control => {
                self.input.ctrl = pressed;
                return;
            }
            Key::Shift => {
                self.input.shift = pressed;
                return;
            }
            _ => {}
        }
        if !pressed {
            return;
        }
        self.dirty = true;

        // A context menu takes the keys first: Escape and the arrows steer it,
        // and Enter chooses. Any other key dismisses it, then does its own work.
        if self.menu_open() {
            match key {
                Key::Escape => {
                    self.close_menu();
                    return;
                }
                Key::Up => {
                    self.move_menu_focus(-1);
                    return;
                }
                Key::Down => {
                    self.move_menu_focus(1);
                    return;
                }
                Key::Enter => {
                    if let Some(action) = self.focused_menu_action() {
                        self.close_menu();
                        self.run_menu_action(action);
                    }
                    return;
                }
                _ => self.close_menu(),
            }
        }

        // Escape is the sheet's Cancel, and it is not a plain dismissal: an
        // import is only ever ended by one of its own buttons.
        if matches!(key, Key::Escape) && matches!(self.modal, Some(Modal::Import(_))) {
            self.cancel_import();
            return;
        }
        if let Some(modal) = &mut self.modal {
            match key {
                Key::Escape => self.modal = None,
                Key::Backspace if matches!(modal, Modal::Password(_)) => {
                    if let Modal::Password(session) = modal
                        && !session.busy
                    {
                        session.typed_mut().pop();
                    }
                }
                Key::Tab => {
                    if let Modal::Password(session) = modal
                        && session.mode == PasswordMode::Create
                        && !session.busy
                    {
                        session.field = match session.field {
                            PasswordField::Password => PasswordField::Confirm,
                            PasswordField::Confirm => PasswordField::Password,
                        };
                    }
                }
                Key::Backspace => {
                    if let Modal::AddToCollection {
                        name, query, focus, ..
                    } = modal
                    {
                        match focus {
                            SheetField::Name => {
                                name.pop();
                            }
                            SheetField::Search => {
                                query.pop();
                                self.sheet_scroll = 0.0;
                            }
                            SheetField::Idle => {}
                        }
                    }
                }
                Key::Enter => self.confirm_modal(),
                _ => {}
            }
            return;
        }

        if self.search_focus && !matches!(key, Key::Enter | Key::Escape | Key::Backspace) {
            return;
        }

        if let Some(viewer) = &mut self.viewer {
            match key {
                Key::Escape => viewer.closing = true,
                Key::Left if self.input.shift => self.viewer_seek(-SEEK_STEP_MS),
                Key::Left => self.viewer_step(-1),
                Key::Right if self.input.shift => self.viewer_seek(SEEK_STEP_MS),
                Key::Right => self.viewer_step(1),
                Key::Up => self.viewer_volume(VOLUME_STEP),
                Key::Down => self.viewer_volume(-VOLUME_STEP),
                Key::Char(' ') => self.toggle_playback(),
                Key::Char('m') | Key::Char('M') => self.toggle_mute(),
                // Zoom needs the media area, which only drawing knows, so the
                // key records the step and the next frame applies it.
                Key::Char('+') | Key::Char('=') => viewer.zoom_step = Some(ZoomStep::In),
                Key::Char('-') => viewer.zoom_step = Some(ZoomStep::Out),
                Key::Char('0') => viewer.zoom_step = Some(ZoomStep::Reset),
                Key::Char('i') | Key::Char('I') => {
                    viewer.info_open = !viewer.info_open;
                }
                Key::Delete => {
                    self.ask_trash_viewer();
                }
                _ => {}
            }
            return;
        }

        match key {
            Key::Escape => {
                if self.search_focus || !self.search.is_empty() {
                    self.clear_search();
                    self.search_focus = false;
                } else if self.select_mode {
                    self.end_selection();
                }
            }
            Key::Enter => {
                if self.search_focus {
                    self.search_focus = false;
                    if self.search_pending {
                        self.dispatch_search();
                    }
                }
            }
            Key::Backspace => {
                if self.search_focus && self.search.pop().is_some() {
                    self.filter_display();
                }
            }
            Key::Delete => self.ask_trash(self.selection_ids()),
            Key::Char('a') | Key::Char('A') if self.input.ctrl => {
                self.select_mode = true;
                // With no search, select every item in the filter (the
                // catalogue answer), not only the rows loaded so far.
                let ids: Vec<String> = if self.search.trim().is_empty() {
                    self.timeline_filter()
                        .and_then(|filter| self.library.item_ids(&filter).ok())
                        .unwrap_or_default()
                } else {
                    self.display.iter().map(|item| item.id.clone()).collect()
                };
                self.selection = ids.into_iter().collect();
                self.sync_select_mode();
            }
            Key::Char('f') | Key::Char('F') if self.input.ctrl => {
                self.search_focus = true;
                self.search_blink = 0.0;
            }
            Key::Char('/') => {
                self.search_focus = true;
                self.search_blink = 0.0;
            }
            Key::Char('o') | Key::Char('O') if self.input.ctrl => self.choose_import_source(),
            Key::PageDown => {
                self.scroll_target =
                    (self.scroll_target + self.scroll_max * 0.16).min(self.scroll_max)
            }
            Key::PageUp => {
                self.scroll_target = (self.scroll_target - self.scroll_max * 0.16).max(0.0)
            }
            Key::Home => self.scroll_target = 0.0,
            Key::End => self.scroll_target = self.scroll_max,
            _ => {}
        }
    }

    pub fn dropped(&mut self, paths: Vec<PathBuf>) {
        self.import(paths);
    }

    pub(crate) fn check_import_dialogs(&mut self) -> Result<(), String> {
        let output = std::env::var_os("FORBIDDEN_LIBRARY_RENDERCHECK_DIR").map(PathBuf::from);
        self.choose_import_source();
        for mode in 0..3 {
            if mode == 1 {
                // Render checks never expose a listener to the LAN.
                let session = crate::remote_import::Session::bind(
                    "127.0.0.1".parse().unwrap(),
                    Duration::from_secs(30),
                    Arc::new(|| {}),
                )?;
                self.modal = Some(Modal::RemoteImport(session));
            } else if mode == 2
                && let Some(Modal::RemoteImport(session)) = &self.modal
            {
                self.modal = Some(Modal::RemoteImportDetails(session.clone()));
            }
            self.modal_t = 1.0;
            for _ in 0..6 {
                self.render(Instant::now());
            }
            let (pixels, width, height) = crate::capture(self)?;
            crate::save_capture(
                output.as_deref(),
                match mode {
                    0 => "import-source",
                    1 => "import-qr",
                    _ => "import-connection-details",
                },
                &pixels,
                width,
                height,
            )?;
        }
        self.modal = None;
        Ok(())
    }

    fn choose_import_source(&mut self) {
        self.modal = Some(Modal::ImportSource);
        self.modal_t = 0.0;
        self.dirty = true;
    }

    fn pick_files(&mut self) {
        let tx = self.events_tx.clone();
        let proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name("forbiddenlibrary-picker".into())
            .spawn(move || {
                let files = rfd::FileDialog::new()
                    .add_filter(
                        "Photos, videos and audio",
                        &[
                            "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic",
                            "heif", "avif", "jxl", "dng", "cr2", "nef", "arw", "mp4", "mov", "m4v",
                            "mkv", "webm", "avi", "wmv", "mpg", "mpeg", "m2ts", "mts", "ts", "3gp",
                            "flv", "apng", "mp3", "m4a", "m4b", "aac", "flac", "wav", "aif",
                            "aiff", "aifc", "ogg", "oga", "opus", "wma", "ape", "wv", "mpc", "tta",
                        ],
                    )
                    .pick_files()
                    .unwrap_or_default();
                if !files.is_empty() {
                    let _ = tx.send(AppEvent::Paths(files));
                    if let Some(proxy) = proxy {
                        let _ = proxy.send_event(());
                    }
                }
            })
            .ok();
    }

    /// The optimizer's settings for an import, or `None` when it is off.
    fn import_optimize(&self) -> Option<OptimizeConfig> {
        self.settings
            .optimize_on_import
            .then(OptimizeConfig::default)
    }

    pub fn import(&mut self, paths: Vec<PathBuf>) {
        self.import_staged(paths, None);
    }

    fn import_staged(&mut self, paths: Vec<PathBuf>, staging: Option<Arc<tempfile::TempDir>>) {
        if paths.is_empty() {
            return;
        }
        // One import is on its sheet at a time. Files dropped meanwhile wait
        // for the sheet to close rather than being lost.
        if let Some(Modal::Import(session)) = self.modal.as_mut() {
            session.queued.extend(paths);
            self.dirty = true;
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.modal = Some(Modal::Import(ImportSession {
            phase: ImportPhase::Running,
            total: 0,
            done: 0,
            current: "Looking for media…".into(),
            cancel: Arc::clone(&cancel),
            report: None,
            error: None,
            queued: Vec::new(),
            finished_at: None,
        }));
        self.modal_t = 0.0;
        if let (Some(pool), Some(proxy)) = (self.texture_pool.as_ref(), self.proxy.clone()) {
            let _ = pool;
            worker::start_import(
                Arc::clone(&self.library),
                paths,
                cancel,
                self.import_optimize(),
                self.events_tx.clone(),
                proxy,
                staging,
            );
        }
        self.dirty = true;
    }

    // --- Catalogue -------------------------------------------------------

    /// The catalogue filter for the page on screen.
    fn timeline_filter(&self) -> Option<TimelineFilter> {
        match self.page {
            Page::Media => Some(TimelineFilter::default()),
            Page::Trash => Some(TimelineFilter {
                trashed: true,
                ..Default::default()
            }),
            Page::Collection(id) => Some(TimelineFilter {
                collection_id: Some(id),
                ..Default::default()
            }),
            _ => None,
        }
    }

    /// The music pages are not paged. Every track is read at once: a library's
    /// music is far smaller than its photos, and collections are only known once
    /// every track has been grouped. The Albums page keeps the groups; an open
    /// album keeps just its own tracks, in album order.
    fn load_music(&mut self) {
        match self.library.music() {
            Ok(tracks) => {
                self.albums = albums(
                    tracks
                        .iter()
                        .map(|(item, audio)| (item.id.as_str(), audio.as_ref())),
                );
                let open = match &self.page {
                    Page::Album(key) => Some(key.clone()),
                    _ => None,
                };
                for (item, audio) in tracks {
                    let key = audio.as_ref().and_then(album_key);
                    if let Some(audio) = audio {
                        self.track_tags.insert(item.id.clone(), audio);
                    }
                    if open.is_some() && key == open {
                        self.items.push(item);
                    }
                }
                // The Media page loads its own timeline, so only an open album
                // sets the count here.
                if open.is_some() {
                    self.timeline_total = self.items.len();
                }
            }
            Err(error) => self.last_error = Some(error.to_string()),
        }
    }

    /// The music tags of an item, from the cache or the catalogue. `None` for
    /// anything that is not music.
    fn track_meta(&mut self, item: &Item) -> Option<AudioMeta> {
        if item.kind != Kind::Audio {
            return None;
        }
        if let Some(audio) = self.track_tags.get(&item.id) {
            return Some(audio.clone());
        }
        let audio = self.library.audio_meta(&item.id).ok().flatten()?;
        self.track_tags.insert(item.id.clone(), audio.clone());
        Some(audio)
    }

    pub fn reload(&mut self) {
        // Visiting (or refreshing) the trash is also when stale items go.
        if self.page == Page::Trash {
            let _ = self.library.purge_expired_trash();
        }
        // Catalogue totals, not a filesystem walk: this runs on every page
        // switch and import, so it has to stay well under a frame. The one
        // stat per file belongs to the verifier.
        self.stats = self.library.stats().unwrap_or_default();
        self.collections = self.library.collections().unwrap_or_default();
        // The page opens on one bounded read. Everything past it arrives
        // when the scroll gets near the end (see `load_more_timeline`).
        self.items.clear();
        self.timeline_total = 0;
        self.timeline_cursor = None;
        self.timeline_done = true;
        self.capture_index = None;
        if matches!(self.page, Page::Albums | Page::Album(_)) {
            self.load_music();
        } else if let Some(filter) = self.timeline_filter() {
            match self.library.item_page(&filter, None, TIMELINE_PAGE) {
                Ok(page) => {
                    self.items = page.items;
                    self.timeline_total = page.total.unwrap_or(0);
                    self.timeline_cursor = page.cursor;
                    self.timeline_done = page.done;
                }
                Err(error) => self.last_error = Some(error.to_string()),
            }
        }
        // The Media page shows each album once, so it needs the albums too.
        if self.page == Page::Media {
            self.load_music();
        }
        // Every cover, of collections and albums alike, keyed the way its tile looks it up.
        let covers_of: Vec<(String, Vec<String>)> = self
            .collections
            .iter()
            .map(|collection| (collection.id.to_string(), collection.cover_item_ids.clone()))
            .chain(
                self.albums
                    .iter()
                    .map(|album| (album.key.clone(), album.cover_item_ids.clone())),
            )
            .collect();
        self.covers.clear();
        let cover_ids: Vec<String> = covers_of
            .iter()
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect();
        if !cover_ids.is_empty() {
            let fetched = self.library.items_by_ids(&cover_ids).unwrap_or_default();
            let by_id: HashMap<&str, &Item> = fetched
                .iter()
                .map(|item| (item.id.as_str(), item))
                .collect();
            for (key, ids) in &covers_of {
                let layers: Vec<Item> = ids
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()).map(|item| (*item).clone()))
                    .collect();
                if !layers.is_empty() {
                    self.covers.insert(key.clone(), layers);
                }
            }
        }
        // An album that has lost its last track, or a collection that was
        // deleted, has nothing left to show: its page closes to the list.
        match &self.page {
            Page::Album(key) if !self.albums.iter().any(|album| &album.key == key) => {
                self.page = Page::Albums;
            }
            Page::Collection(id)
                if !self
                    .collections
                    .iter()
                    .any(|collection| collection.id == *id) =>
            {
                self.page = Page::Collections;
            }
            _ => {}
        }
        // A selection made before a reload is only provably still present
        // once the whole page is loaded; otherwise its rows are simply not in
        // memory yet and dropping them would be wrong.
        if self.timeline_done {
            self.selection
                .retain(|id| self.items.iter().any(|item| &item.id == id));
        }
        self.sync_select_mode();
        self.filter_display();
    }

    /// Extend the loaded prefix. Called while scrolling, before the end of the
    /// loaded rows comes into view, so the next page is usually already there.
    fn load_more_timeline(&mut self) {
        if self.timeline_done || !self.search.trim().is_empty() {
            return;
        }
        let Some(filter) = self.timeline_filter() else {
            return;
        };
        let Some(cursor) = self.timeline_cursor.clone() else {
            self.timeline_done = true;
            return;
        };
        match self
            .library
            .item_page(&filter, Some(&cursor), TIMELINE_PAGE)
        {
            Ok(page) if page.items.is_empty() => self.timeline_done = true,
            Ok(page) => {
                // Later pages leave the total as the first page reported it.
                if let Some(total) = page.total {
                    self.timeline_total = total;
                }
                self.timeline_cursor = page.cursor;
                self.timeline_done = page.done;
                if self.search.trim().is_empty() {
                    self.display.extend(page.items.iter().cloned());
                    self.collapse_albums();
                }
                self.items.extend(page.items);
                self.dirty = true;
            }
            Err(error) => {
                self.timeline_done = true;
                self.last_error = Some(error.to_string());
            }
        }
    }

    /// The item with this id, from whichever collection currently holds it.
    fn item_by_id(&self, id: &str) -> Option<Item> {
        self.display
            .iter()
            .chain(self.items.iter())
            .chain(self.search_items.iter())
            .find(|item| item.id == id)
            .cloned()
    }

    /// Fetch ids in rank order, bounded by [`SEARCH_LIMIT`].
    ///
    /// The semantic index sees the whole media catalogue, while the page may
    /// be the trash or one collection, so hits are filtered back to what the
    /// page would have shown when it held every row.
    fn items_by_ranked_ids(&self, ids: &[String]) -> Vec<Item> {
        let ids: Vec<String> = ids.iter().take(SEARCH_LIMIT).cloned().collect();
        let mut by_id: HashMap<String, Item> = self
            .library
            .items_by_ids(&ids)
            .unwrap_or_default()
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect();
        let mut ordered: Vec<Item> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
        match self.timeline_filter() {
            Some(filter) if filter.trashed => ordered.retain(|item| item.deleted_at.is_some()),
            Some(filter) if filter.collection_id.is_some() => {
                let allowed: HashSet<String> = self
                    .library
                    .item_ids(&filter)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                ordered.retain(|item| allowed.contains(&item.id));
            }
            Some(_) => ordered.retain(|item| item.deleted_at.is_none()),
            None => {}
        }
        ordered
    }

    /// `(id, captured_at)` for the page's filter. One narrow read, shared by
    /// every keystroke of a date search.
    fn capture_times(&mut self) -> &[(String, i64)] {
        if self.capture_index.is_none() {
            // Collections and the trash keep their own filter; the Albums
            // page has no timeline, so its date search reads the media
            // catalogue (that is what collection matching needs).
            let filter = self.timeline_filter().unwrap_or_default();
            let rows = self.library.capture_index(&filter).unwrap_or_default();
            self.capture_index = Some(rows);
        }
        self.capture_index.as_deref().unwrap_or_default()
    }

    /// Items whose capture date matches a typed date, fetched by id so the
    /// result reaches the whole library rather than only the loaded page.
    fn date_matches(&mut self, query: &str) -> Vec<Item> {
        let needle = query.to_lowercase();
        let checked: Vec<String> = {
            let times = self.capture_times();
            times
                .iter()
                .filter(|(_, captured_at)| {
                    archive_core::format_date(*captured_at)
                        .to_lowercase()
                        .contains(&needle)
                        || archive_core::date_key(*captured_at).contains(&needle)
                })
                .map(|(id, _)| id.clone())
                .take(SEARCH_LIMIT)
                .collect()
        };
        if checked.is_empty() {
            return Vec::new();
        }
        let mut by_id: HashMap<String, Item> = self
            .library
            .items_by_ids(&checked)
            .unwrap_or_default()
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect();
        checked.iter().filter_map(|id| by_id.remove(id)).collect()
    }

    fn filter_display(&mut self) {
        let query = self.search.trim().to_string();
        let mut needs_search = false;
        // Rebuild into the buffer the previous frame used instead of
        // allocating a fresh copy of the catalogue on every keystroke.
        if query.is_empty() {
            self.search_hits = None;
            self.search_items.clear();
            self.display.clear();
            self.display.extend_from_slice(&self.items);
        } else if is_date_query(&query) {
            let matched = self.date_matches(&query);
            self.display.clear();
            self.display.extend(matched);
        } else {
            let ranked = self
                .search_hits
                .as_ref()
                .is_some_and(|(hit, _)| hit.eq_ignore_ascii_case(&query));
            if ranked {
                // Search results are fetched by id, so a search sees every
                // item in the library, not only the loaded timeline page.
                self.display.clear();
                self.display.extend_from_slice(&self.search_items);
            } else {
                let in_flight = self
                    .search_inflight
                    .as_ref()
                    .is_some_and(|(_, sent)| sent.eq_ignore_ascii_case(&query));
                needs_search = !in_flight;
                // Hold the last results until the new ones land. Blanking the
                // grid on every keystroke makes the whole page strobe.
            }
        }
        if needs_search {
            self.schedule_search();
        }
        self.cover_tiles = match self.page {
            Page::Albums => self.matching_albums(&query),
            Page::Collections => self
                .matching_collections(&query)
                .iter()
                .map(|collection| CoverTile {
                    cover_key: collection.id.to_string(),
                    name: collection.name.clone(),
                    count_label: if collection.lock == LockState::Locked {
                        "Locked".into()
                    } else {
                        count_label(collection.count, "item")
                    },
                    page: Page::Collection(collection.id),
                    lock: collection.lock,
                })
                .collect(),
            _ => Vec::new(),
        };
        self.collapse_albums();
        self.invalidate_grid();
        self.scroll_max = (self.grid.height - 100.0).max(0.0);
    }

    /// On the Media page an album of several songs is one tile. Its first song
    /// in the timeline stands for the whole album; the rest leave the grid.
    fn collapse_albums(&mut self) {
        if self.page != Page::Media {
            return;
        }
        let multi: HashSet<&str> = self
            .albums
            .iter()
            .filter(|album| album.count > 1)
            .map(|album| album.key.as_str())
            .collect();
        let track_tags = &self.track_tags;
        let mut shown: HashSet<String> = HashSet::new();
        self.display.retain(|item| {
            let Some(key) = track_tags.get(&item.id).and_then(album_key) else {
                return true;
            };
            !multi.contains(key.as_str()) || shown.insert(key)
        });
    }

    /// The album a Media-page tile stands for, when the track is one of several
    /// songs from its album. `None` for everything else.
    fn media_album(&self, item: &Item) -> Option<&Album> {
        if self.page != Page::Media || item.kind != Kind::Audio {
            return None;
        }
        let key = album_key(self.track_tags.get(&item.id)?)?;
        self.albums
            .iter()
            .find(|album| album.key == key && album.count > 1)
    }

    /// The tiles the Albums page should show: every music album, or those whose
    /// name contains the query, ignoring case.
    fn matching_albums(&self, query: &str) -> Vec<CoverTile> {
        let needle = query.trim().to_lowercase();
        self.albums
            .iter()
            .filter(|album| needle.is_empty() || album.name.to_lowercase().contains(&needle))
            .map(|album| CoverTile {
                cover_key: album.key.clone(),
                name: album.name.clone(),
                count_label: count_label(album.count, "song"),
                page: Page::Album(album.key.clone()),
                lock: LockState::None,
            })
            .collect()
    }

    /// The collections the Collections page should show. With no query that is
    /// every collection; otherwise a collection matches when its own name
    /// contains the query, or when any image inside it does. Images are matched
    /// the same way the Media page matches them: a date query against capture
    /// dates, anything else through the semantic index.
    fn matching_collections(&mut self, query: &str) -> Vec<Collection> {
        let query = query.trim();
        if query.is_empty() {
            return self.collections.clone();
        }
        let needle = query.to_lowercase();
        let mut matched_collections: HashSet<i64> = HashSet::new();
        let item_ids: Vec<String> = if is_date_query(query) {
            // The display already holds the date matches when a timeline is
            // on screen; the Albums page has none and asks the catalogue.
            if self.timeline_filter().is_some() {
                self.display.iter().map(|item| item.id.clone()).collect()
            } else {
                self.date_matches(query)
                    .into_iter()
                    .map(|item| item.id)
                    .collect()
            }
        } else {
            self.search_hits
                .as_ref()
                .filter(|(hit, _)| hit.eq_ignore_ascii_case(query))
                .map(|(_, ids)| ids.clone())
                .unwrap_or_default()
        };
        if !item_ids.is_empty()
            && let Ok(collections) = self.library.collection_ids_for_items(&item_ids)
        {
            matched_collections.extend(collections);
        }
        self.collections
            .iter()
            .filter(|collection| {
                collection.name.to_lowercase().contains(&needle)
                    || matched_collections.contains(&collection.id)
            })
            .cloned()
            .collect()
    }

    fn schedule_search(&mut self) {
        if self.semantic.is_none() {
            return;
        }
        self.search_pending = true;
        self.search_debounce = 0.16;
    }

    fn dispatch_search(&mut self) {
        self.search_pending = false;
        let query = self.search.trim().to_string();
        if query.is_empty() || is_date_query(&query) {
            return;
        }
        if self
            .search_inflight
            .as_ref()
            .is_some_and(|(_, sent)| sent.eq_ignore_ascii_case(&query))
        {
            return;
        }
        let Some(semantic) = self.semantic.as_ref() else {
            return;
        };
        self.search_seq += 1;
        self.search_inflight = Some((self.search_seq, query.clone()));
        semantic.query(self.search_seq, query);
    }

    fn clear_search(&mut self) {
        self.search.clear();
        self.search_seq += 1;
        self.search_inflight = None;
        self.search_hits = None;
        self.search_pending = false;
        self.filter_display();
    }

    /// A text query whose results have not arrived yet.
    fn search_busy(&self) -> bool {
        let query = self.search.trim();
        self.semantic.is_some()
            && !query.is_empty()
            && !is_date_query(query)
            && !self
                .search_hits
                .as_ref()
                .is_some_and(|(hit, _)| hit.eq_ignore_ascii_case(query))
    }

    fn invalidate_grid(&mut self) {
        self.grid_key = (0, 0);
    }

    /// The file name of the item open in the viewer, for the render check.
    pub fn viewer_file_name(&self) -> Option<String> {
        let viewer = self.viewer.as_ref()?;
        let id = viewer.ids.get(viewer.index)?;
        self.item_by_id(id).map(|item| item.original_name)
    }

    /// Open the details panel of the open item, as its Details button does.
    pub fn open_details(&mut self) {
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.info_open = true;
        }
    }

    /// The back arrow of an open album or collection. An album opened from the
    /// Media page returns there, at the same scroll position; otherwise it goes
    /// to its list.
    fn leave_open_page(&mut self) {
        if let Some(resume) = self.media_resume.take() {
            self.set_page(Page::Media);
            self.restore_media(resume);
            return;
        }
        let list = if matches!(self.page, Page::Album(_)) {
            Page::Albums
        } else {
            Page::Collections
        };
        self.set_page(list);
    }

    /// Reads the Media timeline back to the rows it had, then puts the scroll
    /// back where it was. Rows are read in pages, as they were first loaded.
    fn restore_media(&mut self, resume: MediaResume) {
        while self.items.len() < resume.loaded && !self.timeline_done {
            let before = self.items.len();
            self.load_more_timeline();
            if self.items.len() == before {
                break;
            }
        }
        // The grid is rebuilt and the scroll clamped when the page is next drawn.
        self.scroll = resume.scroll;
        self.scroll_target = resume.scroll;
        self.dirty = true;
    }

    pub fn set_page(&mut self, page: Page) {
        if self.page == page {
            return;
        }
        // Opening an album from the Media page remembers the Media page, so the
        // album's back arrow can return to it. Going anywhere else forgets it.
        if matches!(page, Page::Album(_)) {
            if self.page == Page::Media {
                self.media_resume = Some(MediaResume {
                    scroll: self.scroll_target,
                    loaded: self.items.len(),
                });
            }
        } else {
            self.media_resume = None;
        }
        self.page = page;
        self.scroll = 0.0;
        self.scroll_target = 0.0;
        self.scroll_max = 0.0;
        self.end_selection();
        self.press = None;
        self.hover = None;
        self.close_viewer();
        // A menu's rows were built for the page it opened on.
        self.close_menu();
        self.modal = None;
        // Messages belong to their page: a page change starts a fresh one
        // rather than dissolving across the old page's.
        self.empty_state = None;
        self.empty_prev = None;
        self.empty_t = 1.0;
        // Thumbnails stay resident across page switches: the LRU in
        // `ThumbCache` already caps VRAM, and clearing here made every visible
        // tile fall back to its colour placeholder while it reloaded.
        self.search_focus = false;
        self.reload();
        self.dirty = true;
    }

    // --- Events from workers --------------------------------------------

    fn drain_events(&mut self) {
        self.upload_ready_textures();
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                AppEvent::RemoteImportChanged => self.on_remote_import_changed(),
                AppEvent::Import(event) => self.on_import_event(event),
                AppEvent::ImportDone(report) => self.on_import_done(report),
                AppEvent::ImportDiscarded(result) => self.on_import_discarded(result),
                AppEvent::Vault(done) => self.on_vault_done(done),
                AppEvent::Texture {
                    key,
                    id,
                    kind,
                    epoch,
                    result,
                } => {
                    let ready = ReadyTexture {
                        key,
                        id,
                        kind,
                        epoch,
                        result,
                    };
                    if kind == TexKind::Thumb {
                        self.grid_textures.push_back(ready);
                    } else {
                        self.viewer_textures.push_back(ready);
                    }
                }
                AppEvent::ThumbRebuilt { id, result } => {
                    if result.is_ok() {
                        self.thumb_failed.remove(&id);
                        self.rebuilt.insert(id.clone());
                        self.mark_thumb_ready(&id);
                    } else {
                        self.thumb_failed.insert(id);
                    }
                }
                AppEvent::Paths(paths) => self.import(paths),
                AppEvent::SemanticResults { seq, ids } => {
                    if self
                        .search_inflight
                        .as_ref()
                        .is_some_and(|(sent, _)| *sent == seq)
                    {
                        if let Some((_, query)) = self.search_inflight.take() {
                            // An item locked since the index was read must not come back as a hit.
                            let ids = self.library.visible_ids(&ids).unwrap_or_default();
                            self.search_items = self.items_by_ranked_ids(&ids);
                            self.search_hits = Some((query, ids));
                        }
                        self.filter_display();
                        self.dirty = true;
                    }
                }
                AppEvent::SemanticAdvanced => {
                    let query = self.search.trim().to_string();
                    if !query.is_empty() && !is_date_query(&query) {
                        self.schedule_search();
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// A collection was just sealed. Textures asked for before now may still be
    /// in flight, and their pixels must not reach the screen. See `TextureJob`.
    fn collections_sealed(&mut self) {
        self.vault_epoch += 1;
    }

    /// Upload finished textures for at most `TEXTURE_BUDGET` of a frame. The
    /// viewer's pictures go first, and at least one upload always happens so a
    /// slow one cannot starve the queue.
    fn upload_ready_textures(&mut self) {
        let started = Instant::now();
        while let Some(ready) = self
            .viewer_textures
            .pop_front()
            .or_else(|| self.grid_textures.pop_front())
        {
            self.apply_texture(ready);
            if started.elapsed() >= TEXTURE_BUDGET {
                break;
            }
        }
    }

    /// Drop queued thumbnail jobs for tiles that have scrolled out of the window
    /// (one screen either side of the viewport), so the pool decodes what is
    /// on screen rather than a backlog of old tiles.
    fn prune_thumb_queue(&mut self, viewport: Rect) {
        if (self.scroll - self.pruned_at).abs() < PRUNE_STEP {
            return;
        }
        self.pruned_at = self.scroll;
        let top = self.scroll - viewport.h;
        let bottom = self.scroll + 2.0 * viewport.h;
        let wanted: HashSet<String> = grid::tiles_between(&self.grid.tiles, top, bottom)
            .filter_map(|tile| self.display.get(self.grid.tiles[tile].index))
            .map(|item| TexKind::Thumb.key(&item.id))
            .collect();
        let dropped = match self.texture_pool.as_ref() {
            Some(pool) => pool.retain_thumbs(|key| wanted.contains(key)),
            None => Vec::new(),
        };
        // A dropped key can be asked for again once its tile is back in view.
        for key in dropped {
            self.thumb_pending.remove(&key);
        }
    }

    fn apply_texture(&mut self, ready: ReadyTexture) {
        let ReadyTexture {
            key,
            id,
            kind,
            epoch,
            result,
        } = ready;
        self.thumb_pending.remove(&key);
        self.preview_pending.remove(&key);
        self.preview_started.remove(&key);
        // A decode asked before a lock can finish after it. Its pixels
        // belong to a collection that is now sealed, so they are dropped.
        if epoch != self.vault_epoch {
            return;
        }
        if kind == TexKind::Full {
            self.on_full_texture(&id, result);
            return;
        }
        match result {
            Ok((width, height, pixels)) => {
                if let Some(context) = self.context.as_ref()
                    && let Some(texture) = thumbs::upload(
                        context,
                        "archive preview",
                        width,
                        height,
                        &pixels,
                        // Previews are drawn smaller than decoded, so
                        // they need the full mip chain to avoid aliasing.
                        if kind == TexKind::Preview {
                            None
                        } else {
                            Some(1)
                        },
                    )
                {
                    if kind == TexKind::Preview {
                        self.preview_failed.remove(&id);
                        self.previews.insert(key, texture);
                    } else {
                        self.thumbs.insert(key, texture);
                        self.thumb_failed.remove(&id);
                        self.mark_thumb_ready(&id);
                    }
                }
            }
            Err(_) => {
                if kind == TexKind::Thumb {
                    self.thumb_failed.insert(id);
                } else {
                    // Occasional failures are worth retrying (a file
                    // can be busy while the optimizer rewrites it),
                    // but a broken file should not be decoded on
                    // every frame.
                    let failures = self.preview_failed.entry(id).or_insert((0, Instant::now()));
                    failures.0 = failures.0.saturating_add(1);
                    failures.1 = Instant::now();
                }
            }
        }
    }

    fn on_import_event(&mut self, event: ImportEvent) {
        let Some(Modal::Import(session)) = self.modal.as_mut() else {
            return;
        };
        match event {
            ImportEvent::Scanning => {
                session.current = "Looking for media…".into();
            }
            ImportEvent::Started { total, .. } => {
                session.total = total;
                session.current = if total == 0 {
                    "Nothing to import".into()
                } else {
                    "Copying files safely…".into()
                };
            }
            ImportEvent::FileStarted { index, total, path } => {
                session.done = index;
                session.total = total;
                session.current = file_label(&path);
            }
            ImportEvent::Copying { .. } | ImportEvent::FileDone { .. } => {}
            ImportEvent::FileFailed { path, error } => {
                session.current = format!("{} — {error}", file_label(&path));
            }
            ImportEvent::Finished(report) => {
                session.done = report.imported + report.duplicates + report.failed;
                session.current = "Finishing…".into();
            }
        }
        self.dirty = true;
    }

    /// The copy has finished and the report is in. The sheet now asks for a
    /// decision. A copy that was cancelled part-way is taken back at once,
    /// since nobody is left to review it.
    fn on_import_done(&mut self, report: ImportReport) {
        let Some(Modal::Import(session)) = self.modal.as_mut() else {
            // Nothing is on screen to review it, so the import is kept.
            self.keep_import(report.imported);
            return;
        };
        let stopped = session.phase == ImportPhase::Stopping;
        let created = report.created.clone();
        // Only a problem needs a decision. Otherwise the sheet says so and
        // closes itself (see `tick_import_sheet`).
        let clean = report.failures.is_empty();
        session.report = Some(report);
        session.phase = if clean {
            ImportPhase::Complete
        } else {
            ImportPhase::Review
        };
        if clean {
            session.finished_at = Some(self.time);
        }
        if stopped {
            self.remove_import(created);
        }
        self.dirty = true;
    }

    /// A clean import whose result has been on screen for its hold. Its sheet
    /// is easing out.
    fn import_leaving(&self) -> bool {
        matches!(
            self.modal.as_ref(),
            Some(Modal::Import(session))
                if session
                    .finished_at
                    .is_some_and(|at| self.time - at >= IMPORT_COMPLETE_HOLD)
        )
    }

    /// Closes a finished import's sheet once it has eased out of view, keeping
    /// the import as Confirm would.
    fn tick_import_sheet(&mut self) {
        if self.import_leaving() && self.modal_t <= 0.0 {
            self.confirm_import();
        }
    }

    /// Cancel. Mid-copy it stops the copy; once the copy has been reviewed it
    /// takes the import back. Nothing else ends an import early.
    fn cancel_import(&mut self) {
        let review_ids = match self.modal.as_mut() {
            Some(Modal::Import(session)) => match session.phase {
                ImportPhase::Running => {
                    session.cancel.store(true, Ordering::Relaxed);
                    session.phase = ImportPhase::Stopping;
                    None
                }
                ImportPhase::Review => Some(
                    session
                        .report
                        .as_ref()
                        .map(|report| report.created.clone())
                        .unwrap_or_default(),
                ),
                ImportPhase::Stopping | ImportPhase::Removing | ImportPhase::Complete => None,
            },
            _ => None,
        };
        if let Some(ids) = review_ids {
            self.remove_import(ids);
        }
        self.dirty = true;
    }

    /// Confirm keeps the reviewed import: its media is shown, and the indexing
    /// and optimizing that wait on new media begin.
    fn confirm_import(&mut self) {
        let imported = match self.modal.as_ref() {
            Some(Modal::Import(session)) => {
                session.report.as_ref().map_or(0, |report| report.imported)
            }
            _ => return,
        };
        self.close_import();
        self.keep_import(imported);
    }

    /// Takes an import back: removes the rows it added, off the UI thread. An
    /// import that added nothing has nothing to remove, so the sheet closes.
    fn remove_import(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            self.close_import();
            return;
        }
        if let Some(Modal::Import(session)) = self.modal.as_mut() {
            session.phase = ImportPhase::Removing;
            session.error = None;
        }
        if let Some(proxy) = self.proxy.clone() {
            worker::start_discard(
                Arc::clone(&self.library),
                ids,
                self.events_tx.clone(),
                proxy,
            );
        }
        self.dirty = true;
    }

    fn on_import_discarded(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.close_import(),
            // The sheet stays open on the same decision, so Cancel can retry.
            Err(error) => {
                if let Some(Modal::Import(session)) = self.modal.as_mut() {
                    session.phase = ImportPhase::Review;
                    session.error = Some(format!("Couldn't take the import back: {error}"));
                }
            }
        }
        self.dirty = true;
    }

    /// Closes the import sheet. Files dropped while it was open start now.
    fn close_import(&mut self) {
        let queued = match self.modal.as_mut() {
            Some(Modal::Import(session)) => std::mem::take(&mut session.queued),
            _ => return,
        };
        self.modal = None;
        self.import(queued);
        self.dirty = true;
    }

    /// Keeps `imported` new items. They show in the archive, and the
    /// indexing and optimizing that wait on new media begin.
    fn keep_import(&mut self, imported: usize) {
        self.reload();
        if imported > 0
            && let Some(semantic) = &self.semantic
        {
            semantic.wake();
        }
    }

    fn toast(&mut self, text: &str) {
        self.toast = Some(Toast {
            text: text.to_string(),
            age: 0.0,
        });
        self.dirty = true;
    }

    // --- Animation / frame scheduling -----------------------------------

    fn animate(&mut self, dt: f32) {
        self.time += dt;
        self.nav.step(nav_row(&self.page), dt, CHROME_OMEGA);
        if self.page == Page::Trash {
            self.trash_notice.step(1.0, dt, CHROME_OMEGA);
        } else {
            self.trash_notice = Spring::at(0.0);
        }
        self.opened.step(open_amount(&self.page), dt, CHROME_OMEGA);
        for index in 0..details::BLOCK_COUNT {
            let target = if self.details_open[index] { 1.0 } else { 0.0 };
            self.details_spring[index].step(target, dt, DROP_OMEGA);
        }
        // A short eased entrance keeps the surface and its outline arriving
        // together, with no sudden edge appearing after the card. A finished
        // import leaves the same way: its target drops to zero once the hold
        // is over, and the sheet closes when the ease has run out.
        let modal_target = if self.modal.is_some() && !self.import_leaving() {
            1.0
        } else {
            0.0
        };
        self.modal_t += (modal_target - self.modal_t) * (1.0 - (-dt * 24.0).exp());
        if (modal_target - self.modal_t).abs() < 0.002 {
            self.modal_t = modal_target;
        } else {
            self.animating = true;
        }
        // The sheet's search and plus ease the same way the Media search does.
        // A context menu pops up at once and fades out. Once a closing menu
        // has faded it is dropped.
        if self.menu_open() {
            self.menu_t = 1.0;
        } else {
            self.menu_t = (self.menu_t - dt / MENU_FADE_SECS).max(0.0);
        }
        if self.menu.as_ref().is_some_and(|menu| menu.closing) && self.menu_t <= 0.0 {
            self.menu = None;
        }
        self.sheet_search
            .step(self.sheet_search_target(), dt, SEARCH_OMEGA);
        let create_omega = if self.sheet_create_target() > 0.0 {
            SEARCH_OMEGA
        } else {
            NAME_SHUT_OMEGA
        };
        self.sheet_create
            .step(self.sheet_create_target(), dt, create_omega);
        // A name typed into a field that has shut is dropped only once the
        // field is gone, so it stays on screen while the field closes.
        if let Some(Modal::AddToCollection {
            name,
            creating: false,
            ..
        }) = self.modal.as_mut()
            && self.sheet_create.settled(0.0)
        {
            name.clear();
        }
        if self.sheet_animating() {
            self.animating = true;
        }
        let diff = self.scroll_target - self.scroll;
        if diff.abs() > 0.2 {
            self.scroll += diff * (1.0 - (-dt * 18.0).exp());
        } else {
            self.scroll = self.scroll_target;
        }
        let hover = self.hover.clone();
        for (id, value) in self.hover_anim.iter_mut() {
            let target = if hover.as_deref() == Some(id.as_str()) {
                1.0
            } else {
                0.0
            };
            *value += (target - *value) * (1.0 - (-dt * 18.0).exp());
            if (*value - target).abs() < 0.01 {
                *value = target;
            }
        }
        for (id, value) in self.check_anim.iter_mut() {
            let target = if self.select_mode
                || self.selection.contains(id)
                || hover.as_deref() == Some(id.as_str())
            {
                1.0
            } else {
                0.0
            };
            *value += (target - *value) * (1.0 - (-dt * 18.0).exp());
            if (*value - target).abs() < 0.01 {
                *value = target;
            }
        }
        let mut finished_closing = false;
        if let Some(viewer) = self.viewer.as_mut() {
            let target = if viewer.closing { 0.0 } else { 1.0 };
            let rate = if viewer.closing {
                VIEWER_CLOSE_RATE
            } else {
                VIEWER_OPEN_RATE
            };
            viewer.open += (target - viewer.open) * (1.0 - (-dt * rate).exp());
            // Snap once the remaining motion is under a pixel-worth of alpha,
            // so the viewer reports itself settled and stops asking for frames.
            if (viewer.open - target).abs() < 0.002 {
                viewer.open = target;
            }
            if viewer.closing && viewer.open < 0.01 {
                finished_closing = true;
            }
            let info_target = if viewer.info_open { 1.0 } else { 0.0 };
            viewer.info.step(info_target, dt, DETAILS_OMEGA);
        }
        if finished_closing {
            self.close_viewer();
        }
        self.step_viewer_controls(dt);
        self.thumbs.retire_frame();
        self.previews.retire_frame();
        self.retire_viewer_textures();
        // Open while hovered, selected, or holding text; closed otherwise, so
        // merely passing the pointer over the icon leaves nothing open.
        let open = search_is_open(self.search.is_empty(), self.search_hover, self.search_focus);
        self.search_open.step(open, dt, SEARCH_OMEGA);
        if open == 0.0 && self.search_open.settled(0.0) {
            self.search_focus = false;
        }
        // Only a search that stays unanswered long enough gets a message of
        // its own; quick ones resolve before the notice is due.
        let busy = self.search_busy();
        self.search_wait = if busy { self.search_wait + dt } else { 0.0 };
        if self.search_focus {
            // Real time, not the clamped animation step: the caret is woken
            // twice a second, and a clamped step would slow the blink down.
            self.search_blink += self.frame_real_dt;
        }
        if self.search_pending {
            self.search_debounce -= dt;
            if self.search_debounce <= 0.0 {
                self.dispatch_search();
            }
        }
        if let Some(toast) = self.toast.as_mut() {
            toast.age += dt;
            if toast.age > 4.2 {
                self.toast = None;
            }
        }
        self.tick_import_sheet();
        self.drain_events();
    }

    /// When the caret next turns on or off, while the search field is focused.
    /// The blink is driven by these wakes instead of a frame every refresh, so
    /// a focused field costs two frames a second rather than a continuous loop.
    fn caret_wake(&self) -> Option<Instant> {
        if !self.search_focus {
            return None;
        }
        let phase = self.search_blink % CARET_PERIOD;
        let left = if phase < CARET_ON {
            CARET_ON - phase
        } else {
            CARET_PERIOD - phase
        };
        Some(self.last_frame + Duration::from_secs_f32(left.max(0.001)))
    }

    /// Work that must happen even when no frame is drawn (the window is
    /// minimised or covered): unlocked collections still lock on time.
    pub fn idle_tick(&mut self, now: Instant) {
        self.vault_idle_check(now);
    }

    /// The only wake a hidden window needs: the unlocked-collection timer.
    pub fn hidden_wake(&self) -> Option<Instant> {
        self.vault_wake()
    }

    pub fn wants_frame(&self) -> bool {
        if self.dirty {
            return true;
        }
        if !self.viewer_textures.is_empty() || !self.grid_textures.is_empty() {
            return true;
        }
        if self.viewer_animating() {
            return true;
        }
        // A focused search field is not here: its caret blinks on the wakes
        // from `caret_wake`.
        if self.modal.is_some() || self.toast.is_some() || self.search_pending {
            return true;
        }
        // A menu that is fading out needs frames until it is gone.
        if self.menu_t > 0.0 && self.menu.as_ref().is_some_and(|menu| menu.closing) {
            return true;
        }
        if (self.scroll_target - self.scroll).abs() > 0.4 {
            return true;
        }
        let details_moving = self
            .details_spring
            .iter()
            .zip(self.details_open)
            .any(|(spring, open)| !spring.settled(if open { 1.0 } else { 0.0 }));
        if details_moving {
            return true;
        }
        if !self.nav.settled(nav_row(&self.page)) || !self.opened.settled(open_amount(&self.page)) {
            return true;
        }
        if !self
            .trash_notice
            .settled(if self.page == Page::Trash { 1.0 } else { 0.0 })
        {
            return true;
        }
        // Compare against each animation's target, not a fixed range: a value
        // that has just settled at 1.0 still owes a frame to fade out.
        let hover = self.hover.as_deref();
        if self.hover_anim.iter().any(|(id, value)| {
            let target = if hover == Some(id.as_str()) { 1.0 } else { 0.0 };
            (*value - target).abs() > 0.002
        }) {
            return true;
        }
        let select_mode = self.select_mode;
        if self.check_anim.iter().any(|(id, value)| {
            let target = if select_mode || self.selection.contains(id) || hover == Some(id.as_str())
            {
                1.0
            } else {
                0.0
            };
            (*value - target).abs() > 0.002
        }) {
            return true;
        }
        let search_open =
            search_is_open(self.search.is_empty(), self.search_hover, self.search_focus);
        if !self.search_open.settled(search_open) {
            return true;
        }
        if self.sheet_animating() {
            return true;
        }
        if self.search_busy() && self.search_wait < SEARCH_NOTICE_DELAY {
            return true;
        }
        if self.empty_shown && self.empty_t < 1.0 {
            return true;
        }
        if self.animating {
            return true;
        }
        false
    }

    pub fn render(&mut self, now: Instant) {
        self.frame_real_dt = now.saturating_duration_since(self.last_frame).as_secs_f32();
        let dt = self.frame_real_dt.clamp(0.0, 0.05);
        self.last_frame = now;
        self.advance_viewer(now);
        self.vault_idle_check(now);
        self.animate(dt);
        self.dirty = false;
        let frame = self.build_frame(dt);
        if let Some(renderer) = self.renderer.as_mut() {
            match renderer.render_ui_only(Some(&frame)) {
                Ok(()) => {}
                // The presentation engine had no image in time (the window
                // is not focused, or is covered), so this frame was skipped.
                // It is still owed: keep asking for frames, or the UI stays
                // on the last picture until the next input event.
                Err(dream::gfx::SurfaceError::Timeout) => self.dirty = true,
                // The surface changed size or format under the swapchain:
                // rebuild it and draw again.
                Err(dream::gfx::SurfaceError::Outdated) => {
                    renderer.recreate_swapchain();
                    self.dirty = true;
                }
                Err(error) => {
                    self.last_error = Some(format!("{error:?}"));
                }
            }
        }
        if let Some(ui) = self.ui.as_mut() {
            ui.recycle_frame(frame);
        }
        self.input.pressed = false;
        self.input.released = false;
        self.input.secondary_pressed = false;
        self.input.wheel = 0.0;
        self.input.precise = 0.0;
    }

    pub fn build_frame(&mut self, dt: f32) -> UiFrame {
        self.frame_dt = dt;
        self.animating = false;
        let mut ui = self.ui.take().expect("UI is owned by the app");
        ui.set_scale(self.scale);
        let input = self.input.clone();
        ui.begin(
            UiInput {
                dt,
                mouse: input.mouse,
                mouse_down: input.down,
                mouse_pressed: input.pressed,
                mouse_released: input.released,
                wheel: input.wheel,
                wheel_precise: input.precise,
                ..Default::default()
            },
            self.screen,
        );
        // Each frame starts with no empty state on screen; drawing one marks
        // it, which is how a message knows whether it has anything to replace.
        self.empty_drawn = false;
        self.draw(&mut ui);
        self.empty_shown = self.empty_drawn;
        let frame = ui.end();
        self.ui = Some(ui);
        frame
    }

    // --- Interaction helpers --------------------------------------------

    /// Whether something covers the page and the page's own controls must go
    /// inert. A menu counts while it fades out, so its last frames cannot be
    /// clicked through.
    fn overlay(&self) -> bool {
        self.viewer.is_some() || self.modal.is_some() || self.menu.is_some()
    }

    /// Whether a menu is up and taking input. A menu that is fading out does not.
    fn menu_open(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| !menu.closing)
    }

    /// Opens a context menu at the pointer, replacing any menu already up.
    /// Nothing to offer (a music album, say) shuts any menu already up.
    fn open_menu(&mut self, rows: Vec<MenuRow>) {
        if !rows.iter().any(|row| matches!(row, MenuRow::Item { .. })) {
            self.close_menu();
            return;
        }
        self.menu = Some(ContextMenu {
            at: self.input.mouse,
            rows,
            focus: None,
            closing: false,
        });
        self.dirty = true;
    }

    /// Starts the menu fading out. It is dropped once it has faded.
    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.as_mut() {
            menu.closing = true;
            self.dirty = true;
        }
    }

    /// Moves the keyboard focus to the next or previous row that does
    /// something. With nothing focused yet, Down takes the first row and Up
    /// the last.
    fn move_menu_focus(&mut self, step: i32) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let items: Vec<usize> = menu
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, MenuRow::Item { .. }))
            .map(|(index, _)| index)
            .collect();
        let Some(&first) = items.first() else {
            return;
        };
        let last = *items.last().unwrap_or(&first);
        menu.focus = Some(
            match menu
                .focus
                .and_then(|at| items.iter().position(|&i| i == at))
            {
                Some(position) => {
                    let next = position as i32 + step;
                    items[next.rem_euclid(items.len() as i32) as usize]
                }
                None if step > 0 => first,
                None => last,
            },
        );
        self.dirty = true;
    }

    /// The action of the focused row, if the focus is on one.
    fn focused_menu_action(&self) -> Option<MenuAction> {
        let menu = self.menu.as_ref()?;
        match menu.rows.get(menu.focus?)? {
            MenuRow::Item { action, .. } => Some(action.clone()),
            _ => None,
        }
    }

    fn hovered(&self, rect: Rect) -> bool {
        !self.overlay() && rect.contains(self.input.mouse)
    }

    fn take_press(&mut self, rect: Rect) -> bool {
        if !self.overlay() && self.input.pressed && rect.contains(self.input.mouse) {
            self.input.pressed = false;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Hover for controls inside the modal, which are above the overlay that
    /// makes ordinary controls inert.
    fn modal_hover(&self, rect: Rect) -> bool {
        self.modal.is_some() && rect.contains(self.input.mouse)
    }

    fn modal_take_press(&mut self, rect: Rect) -> bool {
        if self.modal.is_some() && self.input.pressed && rect.contains(self.input.mouse) {
            self.input.pressed = false;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    fn modal_button(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        label: &str,
        tone: ModalTone,
        enabled: bool,
    ) -> bool {
        let hovered = enabled && self.modal_hover(rect);
        let filled = tone != ModalTone::Plain;
        let fill = match tone {
            ModalTone::Plain => self.theme.g2,
            ModalTone::Primary => self.theme.fg,
            ModalTone::Danger => self.theme.danger,
        };
        let bg = if !enabled {
            self.theme.g2
        } else if filled {
            if hovered { fill.lighten(0.08) } else { fill }
        } else if hovered {
            self.theme.g3
        } else {
            self.theme.g2
        };
        ui.paint_round_rect(rect, 10.0, bg);
        // A disabled button keeps its grey fill and greys its label too, so it
        // reads as the same control waiting rather than a different one.
        self.label_center(
            ui,
            rect,
            label,
            13.5,
            FontWeight::Medium,
            if !enabled {
                self.theme.faint
            } else if filled {
                self.theme.accent_text
            } else {
                self.theme.fg
            },
        );
        if hovered {
            self.cursor_hand = true;
        }
        enabled && self.modal_take_press(rect)
    }

    fn text_style(&self, ui: &Ui, size: f32, weight: FontWeight, color: Color) -> LabelStyle {
        LabelStyle::new(ui.fonts().resolve(weight, false))
            .size(size)
            .color(color)
            .line_height(1.18)
            .vcenter(true)
    }

    fn label(
        &self,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        size: f32,
        weight: FontWeight,
        color: Color,
    ) {
        if rect.w <= 1.0 || rect.h <= 1.0 {
            return;
        }
        let style = self.text_style(ui, size, weight, color);
        ui.label(rect, text, style);
    }

    /// Like [`Self::label`], flush with the right edge of `rect`.
    fn label_right(
        &self,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        size: f32,
        weight: FontWeight,
        color: Color,
    ) {
        if rect.w <= 1.0 || rect.h <= 1.0 {
            return;
        }
        let style = self.text_style(ui, size, weight, color).align(Align::Right);
        ui.label(rect, text, style);
    }

    /// Like [`Self::label`], centred horizontally in `rect`.
    fn label_center(
        &self,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        size: f32,
        weight: FontWeight,
        color: Color,
    ) {
        if rect.w <= 1.0 || rect.h <= 1.0 {
            return;
        }
        let style = self
            .text_style(ui, size, weight, color)
            .align(Align::Center);
        ui.label(rect, text, style);
    }

    fn label_ellipsis(
        &self,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        size: f32,
        weight: FontWeight,
        color: Color,
    ) {
        if rect.w <= 1.0 || rect.h <= 1.0 {
            return;
        }
        let style = self.text_style(ui, size, weight, color);
        ui.label_ellipsis(rect, text, style);
    }

    fn icon_button(&mut self, ui: &mut Ui, rect: Rect, icon: AppIcon, active: bool) -> bool {
        self.icon_button_tinted(ui, rect, icon, active, None)
    }

    /// An icon button whose glyph takes an explicit ink. Destructive actions
    /// pass the danger colour so the bin reads as destructive at rest.
    fn icon_button_tinted(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        icon: AppIcon,
        active: bool,
        tint: Option<Color>,
    ) -> bool {
        let hovered = self.hovered(rect);
        if hovered || active {
            let bg = if active { self.theme.g3 } else { self.theme.g2 };
            ui.paint_round_rect(rect, 20.0, bg);
        }
        let color = tint.unwrap_or(if active {
            self.theme.fg
        } else {
            self.theme.muted
        });
        paint_icon(ui, center_in(rect, (24.0, 24.0)), icon, color);
        if hovered {
            self.cursor_hand = true;
        }
        self.take_press(rect)
    }

    // --- Modal actions ---------------------------------------------------

    /// The ids of the selection, in the order the set keeps them.
    fn selection_ids(&self) -> Vec<String> {
        self.selection.iter().cloned().collect()
    }

    /// Asks before moving `ids` to the trash. Nothing is asked for an empty list.
    /// Asks before a collection's lock is removed. An unlocked collection is
    /// confirmed here, and a locked one asks for its password.
    fn ask_remove_lock(&mut self, id: i64) {
        if self.collection_lock(id) == LockState::Locked {
            return self.ask_password(id, PasswordMode::RemoveLock);
        }
        self.modal = Some(Modal::Confirm {
            title: "Remove this collection's lock?".into(),
            body: "Its photos and videos return to the library.".into(),
            confirm: "Remove lock".into(),
            action: ConfirmAction::RemoveLock(id),
        });
        self.modal_t = 0.0;
    }

    fn ask_trash(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        // Items inside an unlocked collection cannot go to the trash, since the trash
        // does not show them. They are deleted for good, after asking.
        if self.in_unlocked_collection() {
            return self.ask_delete(ids);
        }
        let count = ids.len();
        self.modal = Some(Modal::Confirm {
            title: format!(
                "Move {} {} to trash?",
                count,
                if count == 1 { "item" } else { "items" }
            ),
            body: String::new(),
            confirm: "Move to trash".into(),
            action: ConfirmAction::TrashIds(ids),
        });
        self.modal_t = 0.0;
    }

    /// Asks before deleting `ids` for good. Nothing is asked for an empty list.
    fn ask_delete(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        self.modal = Some(Modal::Confirm {
            title: format!(
                "Delete {} {} forever?",
                count,
                if count == 1 { "item" } else { "items" }
            ),
            body: String::new(),
            confirm: "Delete".into(),
            action: ConfirmAction::DeleteForever(ids),
        });
        self.modal_t = 0.0;
    }

    fn ask_delete_collection(&mut self, id: i64) {
        self.modal = Some(Modal::Confirm {
            title: "Delete this collection?".into(),
            body: "Photos and videos stay in your archive.".into(),
            confirm: "Delete collection".into(),
            action: ConfirmAction::DeleteCollection(id),
        });
        self.modal_t = 0.0;
    }

    fn restore_ids(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        match self.library.restore(&ids) {
            Ok(count) => {
                self.end_selection();
                self.close_viewer();
                self.reload();
                self.toast(&format!(
                    "Restored {} {}",
                    count,
                    if count == 1 { "item" } else { "items" }
                ));
            }
            Err(error) => self.toast(&error.to_string()),
        }
    }

    /// Opens the collection sheet for `ids`. With nothing to pick from yet,
    /// it starts on the name field, so the first collection is made by adding
    /// media rather than from a separate create button.
    fn ask_add_to_collection(&mut self, ids: Vec<String>) {
        let skip: HashSet<i64> = self
            .library
            .collections_containing_all(&ids)
            .unwrap_or_default()
            .into_iter()
            .collect();
        // With collections to choose from, typing searches them and the name
        // field stays hidden behind the plus. With none, the first one is named.
        let creating = sheet_offers(&self.collections, &skip, "").is_empty();
        // Opening the sheet leaves the search closed, as the Media search is,
        // and takes focus only after a click. With nothing to pick from, the
        // name field is the one thing to type into, so it has the focus.
        let focus = if creating {
            SheetField::Name
        } else {
            SheetField::Idle
        };
        self.modal = Some(Modal::AddToCollection {
            ids,
            name: String::new(),
            query: String::new(),
            focus,
            creating,
            skip,
        });
        self.modal_t = 0.0;
        self.sheet_scroll = 0.0;
    }

    fn ask_delete_viewer(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let Some(id) = viewer.ids.get(viewer.index).cloned() else {
            return;
        };
        // Items inside an unlocked collection are not in the trash page, so they are
        // deleted for good here, as they are on the trash page.
        let permanent = self.page == Page::Trash || self.in_unlocked_collection();
        self.modal = Some(Modal::Confirm {
            title: if permanent {
                "Delete forever?".into()
            } else {
                "Move to trash?".into()
            },
            body: String::new(),
            confirm: if permanent {
                "Delete".into()
            } else {
                "Move to trash".into()
            },
            action: if permanent {
                ConfirmAction::DeleteForever(vec![id])
            } else {
                ConfirmAction::TrashIds(vec![id])
            },
        });
        self.modal_t = 0.0;
    }

    fn ask_trash_viewer(&mut self) {
        self.ask_delete_viewer();
    }

    // --- Context menu ---------------------------------------------------

    /// The menu for a media tile. A tile in the selection acts on the whole
    /// selection. Any other tile is acted on alone and the selection is
    /// dropped, as it is in a file manager.
    fn tile_menu(&mut self, id: &str) -> Vec<MenuRow> {
        if !self.selection.contains(id) {
            self.end_selection();
        }
        let ids = if self.selection.contains(id) {
            self.selection_ids()
        } else {
            vec![id.to_string()]
        };
        let mut rows = Vec::new();
        if ids.len() > 1 {
            rows.push(MenuRow::Header(format!("{} selected", ids.len())));
        }
        if self.page == Page::Trash {
            rows.push(MenuRow::item(
                "Restore",
                AppIcon::Download,
                false,
                MenuAction::Restore(ids.clone()),
            ));
            rows.push(MenuRow::item(
                "Delete forever",
                AppIcon::Trash,
                true,
                MenuAction::DeleteForever(ids),
            ));
        } else {
            rows.push(MenuRow::item(
                "Add to collection",
                AppIcon::Collections,
                false,
                MenuAction::AddToCollection(ids.clone()),
            ));
            if matches!(self.page, Page::Collection(_)) {
                rows.push(MenuRow::item(
                    "Remove from collection",
                    AppIcon::CircleMinus,
                    false,
                    MenuAction::RemoveFromAlbum(ids.clone()),
                ));
            }
            rows.push(MenuRow::item(
                "Move to trash",
                AppIcon::Trash,
                true,
                MenuAction::Trash(ids),
            ));
        }
        rows
    }

    /// The menu for a collection tile. Music albums are sorted by the library,
    /// so they have no menu.
    fn collection_menu(page: &Page, lock: LockState) -> Vec<MenuRow> {
        let Page::Collection(id) = page else {
            return Vec::new();
        };
        let id = *id;
        match lock {
            LockState::None => vec![
                MenuRow::item(
                    "Lock with a password",
                    AppIcon::Lock,
                    false,
                    MenuAction::StartLock(id),
                ),
                MenuRow::item(
                    "Delete collection",
                    AppIcon::Trash,
                    true,
                    MenuAction::DeleteCollection(id),
                ),
            ],
            // A locked collection cannot be deleted until its items are back in
            // the library, so it offers only the ways to open it.
            LockState::Locked => vec![
                MenuRow::item(
                    "Unlock",
                    AppIcon::LockOpen,
                    false,
                    MenuAction::StartUnlock(id),
                ),
                MenuRow::item(
                    "Remove the lock",
                    AppIcon::LockOpen,
                    false,
                    MenuAction::StartRemoveLock(id),
                ),
            ],
            LockState::Unlocked => vec![
                MenuRow::item("Lock now", AppIcon::Lock, false, MenuAction::LockNow(id)),
                MenuRow::item(
                    "Remove the lock",
                    AppIcon::LockOpen,
                    false,
                    MenuAction::StartRemoveLock(id),
                ),
                MenuRow::item(
                    "Delete collection",
                    AppIcon::Trash,
                    true,
                    MenuAction::DeleteCollection(id),
                ),
            ],
        }
    }

    /// The menu for the picture in the viewer: its bar's actions, and adding
    /// it to a collection, which the bar does not offer.
    fn viewer_menu(&self) -> Vec<MenuRow> {
        let Some(viewer) = self.viewer.as_ref() else {
            return Vec::new();
        };
        let Some(id) = viewer.ids.get(viewer.index).cloned() else {
            return Vec::new();
        };
        let ids = vec![id];
        let mut rows = vec![
            MenuRow::item(
                if viewer.info_open {
                    "Hide details"
                } else {
                    "Show details"
                },
                AppIcon::Info,
                false,
                MenuAction::ToggleDetails,
            ),
            MenuRow::item(
                "Add to collection",
                AppIcon::Collections,
                false,
                MenuAction::AddToCollection(ids.clone()),
            ),
        ];
        if matches!(self.page, Page::Collection(_)) {
            rows.push(MenuRow::item(
                "Remove from collection",
                AppIcon::CircleMinus,
                false,
                MenuAction::RemoveFromAlbum(ids.clone()),
            ));
        }
        if self.page == Page::Trash {
            rows.push(MenuRow::item(
                "Restore",
                AppIcon::Download,
                false,
                MenuAction::Restore(ids.clone()),
            ));
            rows.push(MenuRow::item(
                "Delete forever",
                AppIcon::Trash,
                true,
                MenuAction::DeleteForever(ids),
            ));
        } else {
            rows.push(MenuRow::item(
                "Move to trash",
                AppIcon::Trash,
                true,
                MenuAction::Trash(ids),
            ));
        }
        rows
    }

    fn run_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::ToggleDetails => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.info_open = !viewer.info_open;
                }
            }
            MenuAction::AddToCollection(ids) => self.ask_add_to_collection(ids),
            MenuAction::RemoveFromAlbum(ids) => self.remove_from_album(ids),
            MenuAction::Restore(ids) => self.restore_ids(ids),
            MenuAction::Trash(ids) => self.ask_trash(ids),
            MenuAction::DeleteForever(ids) => self.ask_delete(ids),
            MenuAction::DeleteCollection(id) => self.ask_delete_collection(id),
            MenuAction::StartLock(id) => self.ask_password(id, PasswordMode::Create),
            MenuAction::StartUnlock(id) => self.ask_password(id, PasswordMode::Unlock),
            MenuAction::StartRemoveLock(id) => self.ask_remove_lock(id),
            MenuAction::LockNow(id) => self.lock_now(id),
        }
        self.dirty = true;
    }

    /// Draws the context menu and takes its input. A row the pointer is on is
    /// highlighted, and a press on it runs its action. A press or right-click
    /// off the card only dismisses the menu: whatever lies under it does not
    /// also act.
    fn draw_menu(&mut self, ui: &mut Ui, width: f32, height: f32) {
        let Some(menu) = self.menu.clone() else {
            return;
        };
        // The fade eases at both ends, so the menu does not visibly snap out.
        let linear = self.menu_t.clamp(0.0, 1.0);
        let t = linear * linear * (3.0 - 2.0 * linear);
        let live = !menu.closing;
        let size = context_menu_size(ui, &menu.rows);
        let card = menu_place(menu.at, size, (width, height));
        self.menu_rect = card;
        ui.elevation_shadow_faded(card, MENU_RADIUS, Elevation::Overlay, t);
        ui.paint_round_rect(card, MENU_RADIUS, fade(self.theme.g1, t));

        let rects = menu_row_rects(card, &menu.rows);
        let mut focus = menu.focus;
        let mut chosen = None;
        if live
            && let Some(index) = rects.iter().zip(&menu.rows).position(|(rect, row)| {
                matches!(row, MenuRow::Item { .. }) && rect.contains(self.input.mouse)
            })
        {
            focus = Some(index);
            self.cursor_hand = true;
            if self.input.pressed
                && let MenuRow::Item { action, .. } = &menu.rows[index]
            {
                self.input.pressed = false;
                chosen = Some(action.clone());
            }
        }
        for (index, (row, rect)) in menu.rows.iter().zip(&rects).enumerate() {
            match row {
                MenuRow::Header(text) => {
                    self.label(
                        ui,
                        Rect::new(rect.x + MENU_ICON_X - MENU_PAD, rect.y, rect.w, rect.h),
                        text,
                        12.0,
                        FontWeight::Medium,
                        fade(self.theme.muted, t),
                    );
                }
                MenuRow::Item {
                    label,
                    icon,
                    danger,
                    ..
                } => {
                    if focus == Some(index) {
                        ui.paint_round_rect(*rect, MENU_ROW_RADIUS, fade(self.theme.g2, t));
                    }
                    let ink = fade(
                        if *danger {
                            self.theme.danger
                        } else {
                            self.theme.fg
                        },
                        t,
                    );
                    let icon_at = rect.x + MENU_ICON_X - MENU_PAD;
                    paint_icon(
                        ui,
                        center_in(Rect::new(icon_at, rect.y, 18.0, rect.h), (18.0, 18.0)),
                        *icon,
                        ink,
                    );
                    self.label(
                        ui,
                        Rect::new(
                            rect.x + MENU_LABEL_X - MENU_PAD,
                            rect.y,
                            rect.w - MENU_LABEL_X,
                            rect.h,
                        ),
                        label,
                        13.5,
                        FontWeight::Medium,
                        ink,
                    );
                }
            }
        }
        if let Some(menu) = self.menu.as_mut() {
            menu.focus = focus;
        }

        if let Some(action) = chosen {
            self.close_menu();
            self.run_menu_action(action);
            return;
        }
        if live && !card.contains(self.input.mouse) {
            if self.input.pressed {
                self.input.pressed = false;
                self.close_menu();
            }
            if self.input.secondary_pressed {
                self.input.secondary_pressed = false;
                self.close_menu();
            }
        }
    }

    /// Enter. On the search line it adds to the first collection the search
    /// finds; on the name field it creates the collection.
    fn confirm_modal(&mut self) {
        match self.modal.clone() {
            Some(Modal::ImportSource | Modal::RemoteImport(_) | Modal::RemoteImportDetails(_)) => {}
            None => return,
            Some(Modal::Confirm { action, .. }) => {
                self.modal = None;
                self.run_confirm(action);
            }
            Some(Modal::Password(_)) => self.submit_password(),
            Some(Modal::Import(ImportSession {
                phase: ImportPhase::Review,
                ..
            })) => self.confirm_import(),
            // Enter means nothing on an import that is still copying or
            // being taken back.
            Some(Modal::Import(_)) => {}
            Some(Modal::AddToCollection {
                ids,
                query,
                focus: SheetField::Search,
                skip,
                ..
            }) => {
                // With nothing typed there is no first collection to pick, so
                // Enter does not add to one by accident.
                if query.trim().is_empty() {
                    self.toast("Type to find a collection, or name a new one below");
                } else if let Some(collection_id) = sheet_offers(&self.collections, &skip, &query)
                    .first()
                    .map(|collection| collection.id)
                {
                    self.modal = None;
                    self.add_to_collection(collection_id, &ids);
                } else {
                    self.toast("No collection matches that");
                }
            }
            Some(Modal::AddToCollection {
                focus: SheetField::Name,
                ..
            }) => self.create_from_name(),
            // Nothing is being typed into, so Enter has nothing to do.
            Some(Modal::AddToCollection {
                focus: SheetField::Idle,
                ..
            }) => {}
        }
        self.dirty = true;
    }

    /// Creates the collection named on the sheet and adds the selection to it.
    /// A blank name keeps the sheet open on its name field.
    fn create_from_name(&mut self) {
        let Some(Modal::AddToCollection { ids, name, .. }) = self.modal.clone() else {
            return;
        };
        if name.trim().is_empty() {
            self.focus_sheet(SheetField::Name);
            self.toast("Name the new collection first");
            return;
        }
        match self.library.create_collection(&name) {
            Ok(id) => {
                self.modal = None;
                self.add_to_collection(id, &ids);
                self.collections = self.library.collections().unwrap_or_default();
            }
            Err(error) => self.toast(&error.to_string()),
        }
        self.dirty = true;
    }

    /// Whether the sheet's search is open: it has the focus, or text in it.
    fn sheet_search_open(&self) -> bool {
        matches!(
            &self.modal,
            Some(Modal::AddToCollection { focus, query, .. })
                if *focus == SheetField::Search || !query.trim().is_empty()
        )
    }

    fn sheet_creating(&self) -> bool {
        matches!(
            &self.modal,
            Some(Modal::AddToCollection { creating: true, .. })
        )
    }

    fn sheet_search_target(&self) -> f32 {
        if self.sheet_search_open() { 1.0 } else { 0.0 }
    }

    fn sheet_create_target(&self) -> f32 {
        if self.sheet_creating() { 1.0 } else { 0.0 }
    }

    /// Whether either sheet control is still moving toward where it should be.
    fn sheet_animating(&self) -> bool {
        self.modal.is_some()
            && (!self.sheet_search.settled(self.sheet_search_target())
                || !self.sheet_create.settled(self.sheet_create_target()))
    }

    /// The sheet's clear button: empties the search and keeps it focused.
    fn clear_sheet_query(&mut self) {
        if let Some(Modal::AddToCollection { query, focus, .. }) = self.modal.as_mut() {
            query.clear();
            *focus = SheetField::Search;
        }
        self.sheet_scroll = 0.0;
    }

    /// Moves the typing cursor to one of the sheet's fields.
    fn focus_sheet(&mut self, field: SheetField) {
        if let Some(Modal::AddToCollection { focus, .. }) = self.modal.as_mut() {
            *focus = field;
        }
    }

    fn add_to_collection(&mut self, collection_id: i64, ids: &[String]) {
        match self.library.add_to_collection(collection_id, ids) {
            Ok(added) => {
                self.toast(&format!(
                    "Added {} {} to the collection",
                    added,
                    if added == 1 { "item" } else { "items" }
                ));
                self.end_selection();
                self.reload();
            }
            Err(error) => self.toast(&error.to_string()),
        }
    }

    /// Take the current selection out of the collection being viewed. The
    /// items stay in the archive; only their membership goes.
    fn remove_from_album(&mut self, ids: Vec<String>) {
        let Page::Collection(collection_id) = self.page else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        match self.library.remove_from_collection(collection_id, &ids) {
            Ok(removed) => {
                self.toast(&format!(
                    "Removed {} {} from the collection",
                    removed,
                    if removed == 1 { "item" } else { "items" }
                ));
                self.end_selection();
                self.close_viewer();
                self.reload();
            }
            Err(error) => self.toast(&error.to_string()),
        }
    }

    // --- Password locks ----------------------------------------------------

    /// The lock a collection is in, as the catalogue last reported it.
    fn collection_lock(&self, id: i64) -> LockState {
        self.collections
            .iter()
            .find(|collection| collection.id == id)
            .map_or(LockState::None, |collection| collection.lock)
    }

    /// Input inside the window: pointer, wheel and keys. Restarts the idle
    /// timer that locks unlocked collections.
    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Whether the page on screen is a collection that is unlocked right now.
    fn in_unlocked_collection(&self) -> bool {
        matches!(self.page, Page::Collection(id) if self.collection_lock(id) == LockState::Unlocked)
    }

    fn any_unlocked(&self) -> bool {
        self.collections
            .iter()
            .any(|collection| collection.lock == LockState::Unlocked)
    }

    /// Whether the viewer is showing a still picture from inside a collection.
    /// Looking at one is not idleness, so the idle timer is held while it is up.
    fn viewing_picture_in_collection(&self) -> bool {
        matches!(self.page, Page::Collection(_))
            && self
                .viewer
                .as_ref()
                .and_then(|viewer| viewer.media.as_ref())
                .is_some_and(|media| media.kind() == MediaKind::Still)
    }

    /// Once per frame: lock every unlocked collection once the window has had
    /// no input for [`VAULT_IDLE`].
    fn vault_idle_check(&mut self, now: Instant) {
        if !self.any_unlocked() || self.vault_busy || self.viewing_picture_in_collection() {
            return;
        }
        if now.saturating_duration_since(self.last_activity) >= VAULT_IDLE {
            self.lock_all_collections();
        }
    }

    /// The moment the idle timer fires, for the event loop to wake at.
    fn vault_wake(&self) -> Option<Instant> {
        if !self.any_unlocked() || self.vault_busy || self.viewing_picture_in_collection() {
            return None;
        }
        Some(self.last_activity + VAULT_IDLE)
    }

    /// Seal every unlocked collection, in the background. The viewer closes
    /// first, so no player is still holding a file that is about to be sealed.
    fn lock_all_collections(&mut self) {
        self.close_viewer();
        self.start_vault_job(0, VaultKind::LockAll, None);
    }

    /// Seal one unlocked collection now.
    fn lock_now(&mut self, id: i64) {
        self.close_viewer();
        self.start_vault_job(id, VaultKind::Lock, None);
    }

    /// Runs a lock, unlock or seal on a thread of its own. Argon2 and sealing
    /// take seconds on a large collection, and the interface must not wait for
    /// them. The password, if any, is wiped on that thread once used.
    fn start_vault_job(&mut self, collection: i64, kind: VaultKind, password: Option<String>) {
        if self.vault_busy {
            self.toast("Still working on a lock");
            return;
        }
        self.vault_busy = true;
        // The previous job's thread has reported by now, so this join is immediate.
        if let Some(previous) = self.vault_thread.take() {
            let _ = previous.join();
        }
        let library = Arc::clone(&self.library);
        let events = self.events_tx.clone();
        let proxy = self.proxy.clone();
        self.vault_thread = Some(std::thread::spawn(move || {
            let mut password = password;
            let typed = password.as_deref().unwrap_or("");
            let result = match kind {
                VaultKind::Create => library.create_lock(collection, typed),
                VaultKind::Unlock => library.unlock(collection, typed),
                VaultKind::RemoveLock => match password.as_deref() {
                    Some(typed) => library
                        .unlock(collection, typed)
                        .and_then(|()| library.remove_lock(collection)),
                    None => library.remove_lock(collection),
                },
                VaultKind::Lock => library.lock(collection),
                VaultKind::LockAll => library.lock_all().map(|_| ()),
            };
            if let Some(typed) = password.as_mut() {
                typed.zeroize();
            }
            let _ = events.send(AppEvent::Vault(VaultDone {
                collection,
                kind,
                result,
            }));
            if let Some(proxy) = proxy {
                let _ = proxy.send_event(());
            }
        }));
    }

    fn on_vault_done(&mut self, done: VaultDone) {
        self.vault_busy = false;
        self.dirty = true;
        let dialog_open = matches!(
            &self.modal,
            Some(Modal::Password(session)) if session.collection == done.collection
        );
        match (done.kind, done.result) {
            (VaultKind::Create, Ok(()))
            | (VaultKind::Unlock, Ok(()))
            | (VaultKind::RemoveLock, Ok(())) => {
                if done.kind == VaultKind::Create {
                    self.close_viewer();
                    self.thumbs.clear();
                    self.previews.clear();
                    self.collections_sealed();
                }
                if dialog_open {
                    self.modal = None;
                }
                let message = match done.kind {
                    VaultKind::Create => "Collection locked",
                    VaultKind::Unlock => "Collection unlocked",
                    _ => "Lock removed",
                };
                self.toast(message);
                self.reload();
            }
            (_, Err(archive_core::Error::WrongPassword)) if dialog_open => {
                // Try again: the dialog stays, and only the typed text is cleared.
                if let Some(Modal::Password(session)) = self.modal.as_mut() {
                    session.password.zeroize();
                    session.password.clear();
                    session.busy = false;
                    session.field = PasswordField::Password;
                    session.error = Some("That password does not open this collection.".into());
                }
            }
            (kind, Err(error)) => {
                if dialog_open {
                    self.modal = None;
                }
                let message = match kind {
                    VaultKind::Lock | VaultKind::LockAll => format!("Could not lock: {error}"),
                    _ => error.to_string(),
                };
                self.toast(&message);
                // A failed seal leaves its collection unlocked, so the list must
                // show that it still is.
                self.reload();
            }
            (VaultKind::Lock | VaultKind::LockAll, Ok(())) => {
                self.toast(if done.kind == VaultKind::LockAll {
                    "Locked the open collections"
                } else {
                    "Collection locked"
                });
                // Pictures of a sealed collection must not stay in memory either.
                self.collections_sealed();
                self.thumbs.clear();
                self.previews.clear();
                self.reload();
            }
        }
    }

    /// Opens the password dialog for a collection.
    fn ask_password(&mut self, collection: i64, mode: PasswordMode) {
        self.modal = Some(Modal::Password(PasswordSession {
            collection,
            mode,
            password: String::new(),
            confirm: String::new(),
            field: PasswordField::Password,
            error: None,
            busy: false,
        }));
        self.modal_t = 0.0;
    }

    /// The lock button on the bar: locks an open collection, unlocks a locked
    /// one, and asks for a password to lock an ordinary one.
    fn press_vault_button(&mut self, id: i64) {
        match self.collection_lock(id) {
            LockState::None => self.ask_password(id, PasswordMode::Create),
            LockState::Locked => self.ask_password(id, PasswordMode::Unlock),
            LockState::Unlocked => self.lock_now(id),
        }
    }

    /// Called as the window closes. Every unlocked collection is sealed before the
    /// process ends, and this waits for a seal already running, so no key outlives
    /// the app. A seal that fails leaves its collection unlocked on disk; the next
    /// unlock with its password finds it open and carries on from there.
    pub fn lock_for_exit(&mut self) {
        self.close_viewer();
        self.collections_sealed();
        if let Some(job) = self.vault_thread.take() {
            let _ = job.join();
        }
        if let Err(error) = self.library.lock_all() {
            eprintln!("Forbidden Library could not lock every collection: {error}");
        }
    }

    /// Enter in the password dialog. Checks what can be checked without the
    /// key, then starts the work.
    fn submit_password(&mut self) {
        let Some(Modal::Password(session)) = self.modal.as_mut() else {
            return;
        };
        if session.busy {
            return;
        }
        if self.vault_busy {
            session.error = Some("Still working on another lock. Try again in a moment.".into());
            return;
        }
        if session.mode == PasswordMode::Create {
            if session.password.chars().count() < MIN_PASSWORD_CHARS {
                session.error = Some(format!(
                    "Use at least {MIN_PASSWORD_CHARS} characters for the password."
                ));
                return;
            }
            if session.password != session.confirm {
                session.error = Some("The two passwords do not match.".into());
                return;
            }
        } else if session.password.is_empty() {
            session.error = Some("Enter the password.".into());
            return;
        }
        session.busy = true;
        session.error = None;
        let collection = session.collection;
        let kind = match session.mode {
            PasswordMode::Create => VaultKind::Create,
            PasswordMode::Unlock => VaultKind::Unlock,
            PasswordMode::RemoveLock => VaultKind::RemoveLock,
        };
        // The typed text moves to the job, which wipes it. The confirmation is
        // wiped here, since the dialog keeps existing while the job runs.
        let password = std::mem::take(&mut session.password);
        session.confirm.zeroize();
        session.confirm.clear();
        self.start_vault_job(collection, kind, Some(password));
    }

    fn run_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::RemoveLock(id) => self.start_vault_job(id, VaultKind::RemoveLock, None),
            ConfirmAction::TrashIds(ids) => match self.library.trash(&ids) {
                Ok(count) => {
                    self.toast(&format!(
                        "Moved {} {} to the trash",
                        count,
                        if count == 1 { "item" } else { "items" }
                    ));
                    self.end_selection();
                    self.close_viewer();
                    self.reload();
                }
                Err(error) => self.toast(&error.to_string()),
            },
            ConfirmAction::DeleteForever(ids) => match self.library.delete_forever(&ids) {
                Ok(count) => {
                    self.toast(&format!(
                        "Deleted {} {} forever",
                        count,
                        if count == 1 { "item" } else { "items" }
                    ));
                    self.close_viewer();
                    self.end_selection();
                    self.reload();
                }
                Err(error) => self.toast(&error.to_string()),
            },
            ConfirmAction::EmptyTrash => match self.library.empty_trash() {
                Ok(count) => {
                    self.toast(&format!(
                        "Emptied the trash · {} {} removed",
                        count,
                        if count == 1 { "item" } else { "items" }
                    ));
                    self.reload();
                }
                Err(error) => self.toast(&error.to_string()),
            },
            ConfirmAction::DeleteCollection(id) => match self.library.delete_collection(id) {
                Ok(()) => {
                    self.page = Page::Collections;
                    self.reload();
                    self.toast("Collection deleted");
                }
                Err(error) => self.toast(&error.to_string()),
            },
        }
    }

    /// When a track ends and the next item in the list is music too, play it.
    /// An album therefore runs through in the order it is listed. A track
    /// followed by a photo, or the last track, simply stops.
    fn advance_music(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let ended = viewer.media.as_ref().is_some_and(|media| {
            media.video().is_some_and(|player| {
                matches!(player.status(), PlayerStatus::Ended)
                    && player.info().is_some_and(|info| !info.has_video)
            })
        });
        if !ended {
            return;
        }
        let Some(next) = viewer.ids.get(viewer.index + 1).cloned() else {
            return;
        };
        if self
            .item_by_id(&next)
            .is_some_and(|item| item.kind == Kind::Audio)
        {
            self.viewer_step(1);
        }
    }

    fn viewer_step(&mut self, delta: i32) {
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        if viewer.ids.is_empty() {
            return;
        }
        let next = viewer.index as i32 + delta;
        if next < 0 {
            return;
        }
        if next as usize >= viewer.ids.len() {
            return;
        }
        viewer.index = next as usize;
        viewer.open = 0.55;
        self.details_scroll = 0.0;
        let id = viewer.ids[viewer.index].clone();
        self.preview_failed.remove(&id);
        self.start_media(&id);
        self.request_preview(&id);
        self.warm_viewer_neighbors();
    }

    pub fn open_viewer(&mut self, index: usize) {
        self.search_focus = false;
        let ids: Vec<String> = self.display.iter().map(|item| item.id.clone()).collect();
        if index >= ids.len() {
            return;
        }
        let id = ids[index].clone();
        self.close_viewer();
        self.viewer = Some(Viewer::new(ids, index, Instant::now()));
        self.preview_failed.remove(&id);
        self.start_media(&id);
        self.request_preview(&id);
        self.warm_viewer_neighbors();
        self.dirty = true;
    }

    /// Decode only the images immediately around the current one. This makes
    /// sequential viewing feel instant without walking the whole collection or
    /// retaining an unbounded number of full-size textures.
    fn warm_viewer_neighbors(&mut self) {
        let Some((next, previous)) = self.viewer.as_ref().map(|viewer| {
            (
                viewer.ids.get(viewer.index + 1).cloned(),
                viewer
                    .index
                    .checked_sub(1)
                    .and_then(|index| viewer.ids.get(index))
                    .cloned(),
            )
        }) else {
            return;
        };

        // Keep the current image plus at most two likely destinations in the
        // preview LRU. Prefer the next image, which is the common browse path.
        for id in [next, previous].into_iter().flatten() {
            if self.preview_pending.len() >= 3 {
                break;
            }
            self.request_preview(&id);
        }
    }

    fn request_preview(&mut self, id: &str) {
        let key = TexKind::Preview.key(id);
        if self.previews.contains(&key) {
            return;
        }
        // Rapid navigation can leave obsolete decodes in flight. Let those
        // finish, but keep the queue bounded; the current image is requested
        // again by the viewer on the next frame once a slot opens.
        if !self.preview_pending.contains(&key) && self.preview_pending.len() >= 3 {
            return;
        }
        if let Some((attempts, last)) = self.preview_failed.get(id)
            && (*attempts >= 3 || last.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        // A worker can die or its result can be lost; re-ask after a slow
        // decode instead of waiting forever.
        if let Some(started) = self.preview_started.get(&key) {
            if started.elapsed() < Duration::from_secs(20) {
                return;
            }
            self.preview_pending.remove(&key);
            self.preview_started.remove(&key);
        }
        let Some(item) = self.item_by_id(id) else {
            return;
        };
        let max_side = self.preview_side();
        if let Some(pool) = self.texture_pool.as_ref() {
            self.preview_pending.insert(key.clone());
            self.preview_started.insert(key.clone(), Instant::now());
            pool.request(TextureJob {
                key,
                kind: TexKind::Preview,
                item,
                max_side,
                epoch: self.vault_epoch,
            });
        }
    }

    /// Long edge of the viewer proxy stored by the archive.
    fn preview_side(&self) -> u32 {
        archive_core::VIEWER_SIDE
    }

    fn request_thumb(&mut self, item: &Item) {
        if self.context.is_none() {
            return;
        }
        let key = TexKind::Thumb.key(&item.id);
        if self.thumbs.contains(&key) || self.thumb_pending.contains(&key) {
            return;
        }
        if self.thumb_failed.contains(&item.id) && !self.rebuilt.contains(&item.id) {
            // One rebuild attempt per session; a second failure is shown as a
            // placeholder rather than a loop.
            return;
        }
        let Some(pool) = self.texture_pool.as_ref() else {
            return;
        };
        self.thumb_pending.insert(key.clone());
        pool.request(TextureJob {
            key,
            kind: TexKind::Thumb,
            item: item.clone(),
            max_side: 0,
            epoch: self.vault_epoch,
        });
    }

    fn mark_thumb_ready(&mut self, id: &str) {
        for item in self
            .items
            .iter_mut()
            .chain(self.search_items.iter_mut())
            .chain(self.display.iter_mut())
            .filter(|item| item.id == id)
        {
            item.thumb_state = ThumbState::Ready;
            item.thumb_recipe = archive_core::THUMB_RECIPE;
        }
    }

    // --- Drawing ---------------------------------------------------------

    fn draw(&mut self, ui: &mut Ui) {
        let (width, height) = self.screen;
        if width < 2.0 || height < 2.0 {
            return;
        }
        self.cursor_hand = false;
        ui.rect(Rect::new(0.0, 0.0, width, height), self.theme.bg);
        self.draw_sidebar(ui, height);
        self.draw_topbar(ui, width);
        let content = Rect::new(
            SIDEBAR_W,
            TOPBAR_H,
            (width - SIDEBAR_W).max(0.0),
            (height - TOPBAR_H).max(0.0),
        );
        if content.w > 2.0 {
            // Pages swap instantly: no fade, no slide.
            let alpha = 1.0;
            let dy = 0.0;
            ui.paint_push_clip(content);
            match self.page {
                Page::Album(_) => self.draw_album_page(ui, content, alpha, dy),
                Page::Media | Page::Collection(_) | Page::Trash => {
                    self.draw_media_page(ui, content, alpha, dy);
                }
                Page::Albums | Page::Collections => self.draw_cover_grid(ui, content, alpha, dy),
            }
            ui.paint_pop_clip();
        }
        self.draw_toast(ui, width, height);
        self.draw_viewer(ui, width, height);
        self.draw_modal(ui, width, height);
        self.draw_menu(ui, width, height);
        if self.input.released {
            self.press = None;
        }
        if let Some(active) = self.hover.clone()
            && !self.hover_anim.contains_key(&active)
        {
            self.hover_anim.insert(active, 0.0);
        }
        let hover = self.hover.clone();
        self.hover_anim
            .retain(|id, value| *value > 0.004 || hover.as_deref() == Some(id.as_str()));
        self.check_anim.retain(|id, value| {
            *value > 0.004 || self.selection.contains(id) || hover.as_deref() == Some(id.as_str())
        });
    }

    // --- Chrome ----------------------------------------------------------

    fn draw_sidebar(&mut self, ui: &mut Ui, height: f32) {
        ui.rect(Rect::new(0.0, 0.0, SIDEBAR_W, height), self.theme.bg);

        // A column of bare icons, starting at the top of the window. The rows
        // stay exactly where they are; a rounded island is drawn around them
        // so the three read as one control.
        const ROW: f32 = 44.0;
        const STEP: f32 = 48.0;
        const PAD: f32 = 8.0;
        let start_y = 18.0;
        let nav = [Page::Media, Page::Albums, Page::Collections, Page::Trash];
        let icons = [
            AppIcon::Photos,
            AppIcon::Music,
            AppIcon::Collections,
            AppIcon::Trash,
        ];
        let x = (SIDEBAR_W - ROW) * 0.5;
        let last = start_y + STEP * (nav.len() - 1) as f32 + ROW;
        // Equal gaps: as far from the left wall as from the top edge. The rows
        // keep their positions, so only the island's box moves.
        let inset = x - PAD;
        let island = Rect::new(inset, inset, ROW + PAD * 2.0, last + PAD - inset);
        ui.paint_round_rect(island, 20.0, self.theme.g1);

        for index in 0..nav.len() {
            let row = Rect::new(x, start_y + index as f32 * STEP, ROW, ROW);
            // Hover is a quiet wash, unless the highlight already covers the row.
            if self.hovered(row) && (self.nav.x - index as f32).abs() > 0.5 {
                ui.paint_round_rect(row, 14.0, hover_overlay(self.dark, 0.8));
            }
        }

        // One highlight rides the column instead of snapping between rows. It
        // stretches a touch while it moves, so the glide reads as a slide.
        let stretch = (self.nav.v.abs() * 0.6).min(6.0);
        let pill = Rect::new(
            x,
            start_y + self.nav.x * STEP - stretch,
            ROW,
            ROW + stretch * 2.0,
        );
        ui.paint_round_rect(pill, 14.0, self.theme.g2);

        for (index, page) in nav.iter().enumerate() {
            let row = Rect::new(x, start_y + index as f32 * STEP, ROW, ROW);
            // Ink warms as the highlight arrives and cools as it leaves.
            let focus = (1.0 - (self.nav.x - index as f32).abs()).clamp(0.0, 1.0);
            let ink = self.theme.muted.mix(self.theme.fg, focus);
            paint_icon(ui, center_in(row, (24.0, 24.0)), icons[index], ink);
            if self.hovered(row) {
                self.cursor_hand = true;
            }
            if self.take_press(row) {
                self.set_page(page.clone());
            }
        }
    }

    fn draw_topbar(&mut self, ui: &mut Ui, width: f32) {
        let content_x = SIDEBAR_W;
        let content_w = (width - SIDEBAR_W).max(0.0);
        let content_right = content_x + content_w;
        ui.rect(
            Rect::new(content_x, 0.0, content_w, TOPBAR_H),
            self.theme.bg,
        );
        // Pages that draw no search field must not leave it stuck open.
        self.search_hover = false;
        // A viewer or modal dims the bar rather than erasing it, so the page
        // name and the page's actions stay visible behind the overlay. Every
        // control on it is inert while an overlay is up (`hovered` and
        // `take_press` check `overlay`), and the search field is kept shut by
        // dropping focus and never reporting hover.
        if self.overlay() {
            self.search_focus = false;
        }

        if self.select_mode {
            let close = Rect::new(content_x + 16.0, 13.0, 40.0, 40.0);
            if self.icon_button(ui, close, AppIcon::Close, false) {
                self.end_selection();
            }
            self.label(
                ui,
                Rect::new(content_x + 68.0, 0.0, 240.0, TOPBAR_H),
                &format!("{} selected", self.selection.len()),
                15.0,
                FontWeight::Medium,
                self.theme.fg,
            );

            let mut x = content_right - 20.0 - 40.0;
            // In Trash the far-right button deletes for good and restore sits
            // just left of it. Same colours as the viewer pill: restore leans
            // green, and the destructive actions lean red.
            if self.page == Page::Trash {
                if self.icon_button_tinted(
                    ui,
                    Rect::new(x, 13.0, 40.0, 40.0),
                    AppIcon::Trash,
                    false,
                    Some(self.theme.danger),
                ) && !self.selection.is_empty()
                {
                    self.ask_delete(self.selection_ids());
                }
                x -= 44.0;
            }
            let (trash_icon, trash_tint) = if self.page == Page::Trash {
                (AppIcon::Download, self.theme.ok)
            } else {
                (AppIcon::Trash, self.theme.danger)
            };
            if self.icon_button_tinted(
                ui,
                Rect::new(x, 13.0, 40.0, 40.0),
                trash_icon,
                false,
                Some(trash_tint),
            ) && !self.selection.is_empty()
            {
                if self.page == Page::Trash {
                    self.restore_ids(self.selection_ids());
                } else {
                    self.ask_trash(self.selection_ids());
                }
            }
            let trash_left = x;
            x -= 44.0;
            // Collections hold library items, so the trash has no album to add
            // to and draws no button for one; its select-all label then follows
            // the trash action directly rather than leaving the collection button's slot
            // standing empty between them. Inside a collection, removing from it
            // sits left of the collection button.
            let actions_left = if self.page == Page::Trash {
                trash_left
            } else {
                if matches!(self.page, Page::Collection(_)) {
                    if self.icon_button(
                        ui,
                        Rect::new(x, 13.0, 40.0, 40.0),
                        AppIcon::CircleMinus,
                        false,
                    ) && !self.selection.is_empty()
                    {
                        self.remove_from_album(self.selection_ids());
                    }
                    x -= 44.0;
                }
                if self.icon_button(
                    ui,
                    Rect::new(x, 13.0, 40.0, 40.0),
                    AppIcon::Collections,
                    false,
                ) && !self.selection.is_empty()
                {
                    let ids: Vec<String> = self.selection.iter().cloned().collect();
                    self.ask_add_to_collection(ids);
                }
                x
            };
            x = actions_left - 14.0;
            // Once everything on this page is selected the action flips.
            let all_selected = !self.display.is_empty()
                && self
                    .display
                    .iter()
                    .all(|item| self.selection.contains(&item.id));
            let text = if all_selected {
                "Unselect all"
            } else {
                "Select all"
            };
            let text_w = measure_text_width(ui, text, 13.5);
            let all = Rect::new(x - text_w - 10.0, 13.0, text_w + 20.0, 40.0);
            let hovered = self.hovered(all);
            self.label(
                ui,
                all,
                text,
                13.5,
                FontWeight::Medium,
                if hovered {
                    self.theme.fg
                } else {
                    self.theme.muted
                },
            );
            if hovered {
                self.cursor_hand = true;
            }
            if self.take_press(all) {
                if all_selected {
                    self.selection.clear();
                } else if let Some(filter) = self.timeline_filter() {
                    self.selection = self
                        .library
                        .item_ids(&filter)
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                }
                self.sync_select_mode();
                self.dirty = true;
            }
            return;
        }

        // Primary actions are bare icons: no pills, no outlines.
        let actions = self.top_actions();
        let search_t = self.search_open.x.clamp(0.0, 1.0);
        let slots = top_action_rects(&actions, content_x, content_right, search_t);
        // The page name stops short of the search field when there is one.
        let mut title_limit = content_x + content_w * 0.5;
        if let Some((_, field)) = slots
            .iter()
            .find(|(action, _)| *action == TopAction::Search)
        {
            title_limit = field.x - 24.0;
        }
        let open_name = match &self.page {
            Page::Collection(id) => self
                .collections
                .iter()
                .find(|collection| collection.id == *id)
                .map(|collection| collection.name.clone()),
            Page::Album(key) => self
                .albums
                .iter()
                .find(|album| &album.key == key)
                .map(|album| album.name.clone()),
            _ => None,
        };

        for (action, rect) in slots {
            match action {
                TopAction::Import => {
                    if self.icon_button(ui, rect, AppIcon::Upload, false) {
                        self.choose_import_source();
                    }
                }
                TopAction::Vault(id) => {
                    let lock = self.collection_lock(id);
                    // Unlocked shows a closed padlock in the green `ok` ink, with
                    // no highlight behind it, so it reads as a state, not a toggle.
                    let (icon, tint) = match lock {
                        LockState::Locked => (AppIcon::LockOpen, None),
                        LockState::Unlocked => (AppIcon::Lock, Some(self.theme.ok)),
                        LockState::None => (AppIcon::Lock, None),
                    };
                    if self.icon_button_tinted(ui, rect, icon, false, tint) {
                        self.press_vault_button(id);
                    }
                }
                TopAction::DeleteCollection(id) => {
                    if self.icon_button_tinted(
                        ui,
                        rect,
                        AppIcon::Trash,
                        false,
                        Some(self.theme.danger),
                    ) {
                        self.ask_delete_collection(id);
                    }
                }
                TopAction::EmptyTrash => {
                    if self.icon_button_tinted(
                        ui,
                        rect,
                        AppIcon::Trash,
                        false,
                        Some(self.theme.danger),
                    ) {
                        self.modal = Some(Modal::Confirm {
                            title: "Empty the trash?".into(),
                            body: "This cannot be undone.".into(),
                            confirm: "Empty trash".into(),
                            action: ConfirmAction::EmptyTrash,
                        });
                        self.modal_t = 0.0;
                    }
                }
                TopAction::Search => self.draw_search_field(ui, rect),
            }
        }

        if let Some(name) = open_name {
            self.open_title = name;
        }
        self.draw_header_title(ui, content_x, title_limit);
    }

    /// The buttons this page shows, right to left as they are laid out.
    fn top_actions(&self) -> Vec<TopAction> {
        let mut actions = Vec::new();
        match self.page {
            // Music collections are sorted by the library, so they have no delete.
            Page::Media | Page::Albums | Page::Album(_) | Page::Collections => {
                actions.push(TopAction::Import);
            }
            Page::Collection(id) => {
                let lock = self.collection_lock(id);
                // A locked collection has nothing to import into or delete while it is shut.
                if lock != LockState::Locked {
                    actions.push(TopAction::Import);
                }
                actions.push(TopAction::Vault(id));
                if lock != LockState::Locked {
                    actions.push(TopAction::DeleteCollection(id));
                }
            }
            Page::Trash => {
                if self.stats.trash_items > 0 {
                    actions.push(TopAction::EmptyTrash);
                }
            }
        }
        if !matches!(self.page, Page::Trash) {
            actions.push(TopAction::Search);
        }
        actions
    }

    /// The search field at `field`: a circle at rest that grows into a pill
    /// from its own right edge, so nothing else moves.
    fn draw_search_field(&mut self, ui: &mut Ui, field: Rect) {
        let t = self.search_open.x.clamp(0.0, 1.0);
        self.search_rect = field;
        self.search_hover = !self.overlay() && field.contains(self.input.mouse);
        if self.input.pressed && !field.contains(self.input.mouse) {
            self.search_focus = false;
        }

        // One radius, half the height: a circle at rest, a pill when open.
        // No outline; the fill alone says the field is live.
        if t > 0.002 {
            let fill = (t * 1.8).min(1.0);
            ui.paint_round_rect(field, ACTION_BOX * 0.5, fade(self.theme.g1, fill));
        }
        paint_icon(
            ui,
            Rect::new(field.x + 8.0, field.y + 8.0, 24.0, 24.0),
            AppIcon::Search,
            self.theme.muted,
        );

        // Text fades in only once there is room for it, never squeezed.
        let text_a = ((t - 0.45) / 0.35).clamp(0.0, 1.0);
        let clear_w = if self.search.is_empty() { 0.0 } else { 31.0 };
        let text_x = field.x + 42.0;
        let avail = (field.w - 42.0 - 7.0 - clear_w).max(0.0);
        if text_a > 0.0 && avail > 4.0 {
            ui.paint_push_clip(Rect::new(text_x, field.y, avail, field.h));
            let mut caret_x = text_x;
            if self.search.is_empty() {
                // A focused field shows just the caret, the way any text
                // field behaves; the placeholder is for the resting state.
                if !self.search_focus {
                    self.label(
                        ui,
                        Rect::new(text_x, field.y, avail, field.h),
                        "Search",
                        14.0,
                        FontWeight::Regular,
                        fade(self.theme.muted, text_a),
                    );
                }
            } else {
                let (text_width, _) = ui.measure(
                    &self.search,
                    ui.fonts().resolve(FontWeight::Regular, false),
                    14.0,
                );
                let shift = (text_width - avail).max(0.0);
                self.label(
                    ui,
                    Rect::new(text_x - shift, field.y, text_width.max(avail), field.h),
                    &self.search,
                    14.0,
                    FontWeight::Regular,
                    fade(self.theme.fg, text_a),
                );
                caret_x = text_x + text_width.min(avail) - shift + 1.0;
            }
            if self.search_focus && self.search_blink % CARET_PERIOD < CARET_ON {
                ui.paint_round_rect(
                    Rect::new(caret_x, field.y + 10.0, 1.5, 20.0),
                    0.75,
                    fade(self.theme.fg, text_a),
                );
            }
            ui.paint_pop_clip();
        }

        if !self.search.is_empty() && t > 0.7 {
            let clear_a = ((t - 0.7) / 0.3).clamp(0.0, 1.0);
            let clear = Rect::new(field.max().0 - 31.0, field.y + 8.0, 24.0, 24.0);
            if self.hovered(clear) {
                ui.paint_round_rect(clear, 12.0, fade(self.theme.g3, clear_a));
            }
            paint_icon(
                ui,
                clear.inset(5.0),
                AppIcon::Close,
                fade(self.theme.muted, clear_a),
            );
            if self.hovered(clear) {
                self.cursor_hand = true;
            }
            if self.take_press(clear) {
                self.clear_search();
                self.search_focus = true;
            }
        }
        if self.search_hover {
            self.cursor_hand = true;
        }
        if self.take_press(field) {
            self.search_focus = true;
            self.search_blink = 0.0;
        }
    }

    /// The page name, just right of the island. The names sit on a drum that
    /// turns with the island highlight: the old name rolls out one way and the
    /// new one rolls in from the other, on the same spring as the highlight.
    fn draw_header_title(&mut self, ui: &mut Ui, content_x: f32, limit: f32) {
        // Line the page name up with the trash notice and the image grid's
        // left edge, both of which sit a little further in than the island.
        let x = content_x + 26.0;
        let width = (limit - x).max(80.0);
        let open_t = self.opened.x.clamp(0.0, 1.0);
        let bg = self.theme.bg;
        if matches!(self.page, Page::Album(_) | Page::Collection(_)) {
            self.open_row = nav_row(&self.page) as usize;
        }

        ui.paint_push_clip(Rect::new(x, 0.0, width, TOPBAR_H));
        for (index, title) in NAV_TITLES.iter().enumerate() {
            let distance = index as f32 - self.nav.x;
            let mut ink = bg.mix(self.theme.fg, (1.0 - distance.abs()).clamp(0.0, 1.0));
            let mut dy = distance * TITLE_STEP;
            if index == self.open_row {
                // The nav title rolls away while the open album's name rolls in.
                ink = ink.mix(bg, open_t);
                dy -= open_t * TITLE_STEP;
            }
            self.label(
                ui,
                Rect::new(x, dy, width, TOPBAR_H),
                title,
                TITLE_SIZE,
                FontWeight::Regular,
                ink,
            );
        }

        // An open collection takes the Collections slot. Its name sits clear
        // of the back button at its final x and rolls in on the same drum as
        // the page names: vertical motion only, never a diagonal drift.
        let roll = name_roll(self.open_row as f32, self.nav.x, open_t);
        let name_x = x + 40.0;
        let name_ink = bg.mix(self.theme.fg, open_t);
        self.label_ellipsis(
            ui,
            Rect::new(name_x, roll, (limit - name_x).max(80.0), TOPBAR_H),
            &self.open_title,
            TITLE_SIZE,
            FontWeight::Regular,
            name_ink,
        );
        ui.paint_pop_clip();

        // The back arrow rides the same drum: it rolls in below the name when
        // the collection opens and rolls away with it when the page closes.
        let open = matches!(self.page, Page::Album(_) | Page::Collection(_));
        let back_shown = open || open_t > 0.001;
        if back_shown {
            let back = Rect::new(content_x + 12.0, 13.0 + roll, 40.0, 40.0);
            let hovered = open && self.hovered(back);
            ui.paint_push_clip(Rect::new(0.0, 0.0, limit, TOPBAR_H));
            if hovered {
                ui.paint_round_rect(back, 20.0, fade(self.theme.g2, open_t));
            }
            paint_icon(
                ui,
                center_in(back, (24.0, 24.0)),
                AppIcon::Back,
                fade(self.theme.muted, open_t),
            );
            if hovered {
                self.cursor_hand = true;
            }
            if open && self.take_press(back) {
                self.leave_open_page();
            }
            ui.paint_pop_clip();
        }
    }

    // --- Media pages -----------------------------------------------------

    fn draw_media_page(&mut self, ui: &mut Ui, content: Rect, alpha: f32, dy: f32) {
        let mut top = content.y + 10.0 + dy;
        // The retention note travels with the page name revolver: it arrives
        // from the left and leaves just as cleanly when navigating away.
        let notice = self.trash_notice.x.clamp(0.0, 1.0);
        if notice > 0.002 {
            self.label(
                ui,
                Rect::new(
                    content.x + 26.0 - 18.0 * (1.0 - notice),
                    content.y + 2.0 + dy,
                    content.w - 52.0,
                    22.0,
                ),
                &format!(
                    "Items are deleted permanently after {} days in the trash.",
                    archive_core::TRASH_RETENTION_DAYS
                ),
                12.5,
                FontWeight::Regular,
                fade(self.theme.muted, alpha * notice),
            );
        }
        // Reserve the notice's full row immediately so the Trash grid stays
        // at its intended resting position throughout the notice animation.
        if self.page == Page::Trash {
            top += 22.0;
        }
        let viewport = Rect::new(
            content.x + 24.0,
            top,
            (content.w - 48.0).max(20.0),
            (content.max().1 - top - 14.0).max(0.0),
        );
        if viewport.h < 20.0 {
            return;
        }
        if self.display.is_empty() {
            self.draw_empty(ui, viewport, alpha);
            self.scroll_max = 0.0;
            self.scroll_target = 0.0;
            self.scroll = 0.0;
            return;
        }
        self.draw_grid(ui, viewport, alpha, dy);
    }

    /// An open album: its cover, name and artist at the top, then its
    /// songs as a numbered list. The cover is drawn once, not on every song.
    /// A song plays from its row, and the header and list scroll together.
    fn draw_album_page(&mut self, ui: &mut Ui, content: Rect, alpha: f32, dy: f32) {
        let top = content.y + 10.0 + dy;
        let viewport = Rect::new(
            content.x + 24.0,
            top,
            (content.w - 48.0).max(20.0),
            (content.max().1 - top - 14.0).max(0.0),
        );
        if viewport.h < 20.0 {
            return;
        }
        if self.display.is_empty() {
            self.draw_empty(ui, viewport, alpha);
            self.scroll_max = 0.0;
            self.scroll_target = 0.0;
            self.scroll = 0.0;
            return;
        }

        let album = match &self.page {
            Page::Album(key) => self.albums.iter().find(|album| &album.key == key).cloned(),
            _ => None,
        };
        let name = album
            .as_ref()
            .map_or_else(|| self.open_title.clone(), |album| album.name.clone());
        let count = album
            .as_ref()
            .map_or(self.display.len() as i64, |album| album.count);
        let run_ms = album.as_ref().map_or(0, |album| album.duration_ms);
        let cover = album
            .as_ref()
            .and_then(|album| self.covers.get(&album.key))
            .and_then(|layers| layers.first())
            .cloned();
        let artist = self
            .display
            .first()
            .and_then(|item| self.track_tags.get(&item.id))
            .and_then(|audio| {
                audio
                    .tags
                    .album_artist
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .or_else(|| audio.tags.artist.clone())
            });

        // The header is one cover's height plus its gap, then the rows.
        let list_top = ALBUM_COVER + 28.0;
        let total = list_top + SONG_ROW_H * self.display.len() as f32 + 8.0;
        self.scroll_max = (total - viewport.h).max(0.0);
        self.scroll_target = self.scroll_target.clamp(0.0, self.scroll_max);
        self.scroll = self.scroll.clamp(0.0, self.scroll_max);
        if !self.overlay() && viewport.contains(self.input.mouse) {
            let delta = self.input.wheel + self.input.precise;
            if delta.abs() > 0.01 {
                self.scroll_target = (self.scroll_target - delta).clamp(0.0, self.scroll_max);
            }
        }
        let over_scrollbar = self.scroll_max > 0.5 && self.input.mouse.0 > viewport.max().0 - 14.0;
        let inside = viewport.contains(self.input.mouse) && !over_scrollbar;
        let live = inside && !self.overlay();

        ui.paint_push_clip(viewport);
        let y0 = viewport.y - self.scroll + dy;
        let cover_rect = Rect::new(viewport.x, y0, ALBUM_COVER, ALBUM_COVER);
        self.draw_cover_single(ui, cover_rect, 8.0, cover.as_ref(), alpha, &[]);

        // The text stacks up from the cover's bottom edge, beside it.
        let text_x = cover_rect.max().0 + 28.0;
        let text_w = (viewport.max().0 - text_x).max(80.0);
        let base = cover_rect.max().1;
        let meta = Rect::new(text_x, base - 20.0, text_w, 20.0);
        let artist_rect = Rect::new(text_x, meta.y - 26.0, text_w, 22.0);
        let name_rect = Rect::new(text_x, artist_rect.y - 48.0, text_w, 44.0);
        self.label_ellipsis(
            ui,
            name_rect,
            &name,
            34.0,
            FontWeight::SemiBold,
            fade(self.theme.fg, alpha),
        );
        if let Some(artist) = &artist {
            self.label_ellipsis(
                ui,
                artist_rect,
                artist,
                15.0,
                FontWeight::Medium,
                fade(self.theme.fg, alpha),
            );
        }
        let summary = format!(
            "{} · {}",
            count_label(count, "song"),
            duration_label(run_ms)
        );
        self.label(
            ui,
            meta,
            &summary,
            13.0,
            FontWeight::Regular,
            fade(self.theme.muted, alpha),
        );

        // The songs, in album order. Pressing a row plays it; ctrl and shift
        // select, as on the other pages, and the right button opens its menu.
        for index in 0..self.display.len() {
            let row_y = y0 + list_top + index as f32 * SONG_ROW_H;
            if row_y + SONG_ROW_H < viewport.y || row_y > viewport.max().1 {
                continue;
            }
            let row = Rect::new(
                viewport.x,
                row_y,
                viewport.w.min(SONG_LIST_MAX_W),
                SONG_ROW_H,
            );
            let item = self.display[index].clone();
            let id = item.id.clone();
            let selected = self.selection.contains(&id);
            let hover_here = live && self.hovered(row);
            if hover_here {
                self.cursor_hand = true;
            }
            if selected {
                ui.paint_round_rect(row, 6.0, fade(self.theme.g2, alpha));
            } else if hover_here {
                ui.paint_round_rect(row, 6.0, fade(hover_overlay(self.dark, 1.0), alpha));
            }

            // The track number gives way to a play glyph on hover, and to a
            // check while the song is selected.
            let slot = Rect::new(row.x + 8.0, row.y, 44.0, SONG_ROW_H);
            if selected {
                let mark = center_in(slot, (24.0, 24.0));
                ui.paint_round_rect(mark, 12.0, fade(self.theme.fg, alpha));
                paint_icon(
                    ui,
                    mark.inset(5.0),
                    AppIcon::Check,
                    fade(self.theme.accent_text, alpha),
                );
            } else if hover_here {
                paint_icon(
                    ui,
                    optical_nudge(center_in(slot, (16.0, 16.0)), 1.6),
                    AppIcon::Play,
                    fade(self.theme.fg, alpha),
                );
            } else {
                let number = self
                    .track_tags
                    .get(&id)
                    .and_then(|audio| audio.tags.track_no)
                    .unwrap_or(index as i64 + 1);
                self.label_center(
                    ui,
                    slot,
                    &number.to_string(),
                    14.0,
                    FontWeight::Medium,
                    fade(self.theme.muted, alpha),
                );
            }

            let title = self
                .track_tags
                .get(&id)
                .and_then(|audio| audio.tags.title.clone())
                .unwrap_or_else(|| item.original_name.clone());
            let title_rect = Rect::new(row.x + 60.0, row.y, (row.w - 148.0).max(40.0), SONG_ROW_H);
            self.label_ellipsis(
                ui,
                title_rect,
                &title,
                15.0,
                FontWeight::Medium,
                fade(self.theme.fg, alpha),
            );
            if let Some(ms) = item.duration_ms {
                let length = Rect::new(row.max().0 - 88.0, row.y, 76.0, SONG_ROW_H);
                self.label_right(
                    ui,
                    length,
                    &duration_label(ms),
                    13.0,
                    FontWeight::Regular,
                    fade(self.theme.muted, alpha),
                );
            }

            if self.input.secondary_pressed
                && row.contains(self.input.mouse)
                && inside
                && self.modal.is_none()
                && self.viewer.is_none()
            {
                self.input.secondary_pressed = false;
                let rows = self.tile_menu(&id);
                self.open_menu(rows);
            }
            if live && row.contains(self.input.mouse) && self.input.pressed {
                self.input.pressed = false;
                if self.select_mode || self.input.ctrl || self.input.shift {
                    if self.input.shift {
                        self.select_range(index);
                    } else {
                        self.toggle_selection(&id, index);
                    }
                } else {
                    self.press = Some(Press {
                        id: id.clone(),
                        index,
                        at: self.input.mouse,
                    });
                }
            }
            if live
                && self.input.released
                && self.press.as_ref().is_some_and(|press| press.id == id)
            {
                let press = self.press.take().expect("press was checked");
                let moved = (self.input.mouse.0 - press.at.0).abs()
                    + (self.input.mouse.1 - press.at.1).abs();
                if moved < 6.0 {
                    self.open_viewer(press.index);
                }
            }
        }
        ui.paint_pop_clip();
        self.draw_scrollbar(ui, viewport, alpha);
    }

    /// An empty page says what is missing and nothing else: importing is the
    /// top bar's job, the empty archive needs no instructions, and a search
    /// that finds nothing just says so.
    fn draw_empty(&mut self, ui: &mut Ui, viewport: Rect, alpha: f32) {
        let target = self.empty_target();
        // A change of message dissolves one into the other in place; nothing
        // moves, and the screen is never left blank between the two.
        if self.empty_state != target {
            self.empty_prev = if self.empty_shown {
                self.empty_state
            } else {
                None
            };
            self.empty_state = target;
            self.empty_t = 0.0;
        }
        self.empty_drawn = true;
        let dt = self.frame_dt.clamp(0.0, 0.05);
        self.empty_t = (self.empty_t + dt / EMPTY_FADE).min(1.0);
        // A brisk crossfade with an ease-out: the swap answers the click or
        // keystroke at once, and the ink on screen stays steady throughout
        // instead of blinking out and back in.
        let t = ease_out_cubic(self.empty_t);
        if let Some(prev) = self.empty_prev {
            self.paint_empty(ui, viewport, alpha * (1.0 - t), prev);
        }
        if let Some(kind) = self.empty_state {
            self.paint_empty(ui, viewport, alpha * t, kind);
        }
        if t >= 1.0 {
            self.empty_prev = None;
        }
    }

    /// What the empty centre of the page should say right now. `None` means a
    /// search is still young enough that a message would only flash.
    fn empty_target(&self) -> Option<EmptyKind> {
        if self.search_busy() {
            // Hold whatever is already on screen until the wait is long enough
            // to be worth reporting.
            return if self.search_wait >= SEARCH_NOTICE_DELAY {
                Some(EmptyKind::Searching)
            } else {
                self.empty_state
            };
        }
        Some(if !self.search.trim().is_empty() {
            EmptyKind::NoMatches
        } else {
            match self.page {
                Page::Trash => EmptyKind::Trash,
                Page::Albums => EmptyKind::Albums,
                Page::Collection(id) if self.collection_lock(id) == LockState::Locked => {
                    EmptyKind::Locked
                }
                Page::Album(_) | Page::Collection(_) => EmptyKind::Collection,
                _ => EmptyKind::Media,
            }
        })
    }

    fn paint_empty(&self, ui: &mut Ui, viewport: Rect, alpha: f32, kind: EmptyKind) {
        if alpha <= 0.002 {
            return;
        }
        let page_icon = match self.page {
            Page::Trash => AppIcon::Trash,
            Page::Collection(_) => AppIcon::Collections,
            Page::Albums | Page::Album(_) => AppIcon::Music,
            _ => AppIcon::Photos,
        };
        let icon = match kind {
            // Searching and its outcome both wear the magnifier, so the two
            // states share a shape; only the cross marks the empty result.
            EmptyKind::Searching => AppIcon::Search,
            EmptyKind::NoMatches => AppIcon::SearchX,
            EmptyKind::Locked => AppIcon::Lock,
            _ => page_icon,
        };
        let title = match kind {
            EmptyKind::Searching => "Searching…",
            EmptyKind::NoMatches => "No matches",
            EmptyKind::Trash => "Trash is empty",
            EmptyKind::Collection => "This collection is empty",
            EmptyKind::Locked => "This collection is locked",
            EmptyKind::Media => "No media to show",
            EmptyKind::Albums => "No albums yet",
        };
        let center = center_in(viewport, (520.0, 300.0));
        let group_h = 60.0 + 12.0 + 34.0;
        let top = center.y + ((center.h - group_h) * 0.5).max(0.0);
        paint_icon(
            ui,
            Rect::new(center.center().0 - 30.0, top, 60.0, 60.0),
            icon,
            fade(self.theme.faint, alpha),
        );
        self.label_center(
            ui,
            Rect::new(center.x, top + 72.0, center.w, 34.0),
            title,
            23.0,
            FontWeight::SemiBold,
            fade(self.theme.faint, alpha),
        );
    }

    /// A tile's run time, in the bottom-right corner.
    fn duration_pill(&self, ui: &mut Ui, rect: Rect, ms: i64, alpha: f32, tint_a: u8) {
        let label = duration_label(ms);
        let style = self.text_style(
            ui,
            11.0,
            FontWeight::Medium,
            Color::WHITE.with_alpha(tint_a),
        );
        let (text_width, _) = ui.measure(&label, style.font, 11.0);
        let pill = Rect::new(
            rect.max().0 - text_width - 22.0,
            rect.max().1 - 28.0,
            text_width + 12.0,
            20.0,
        );
        ui.paint_round_rect(pill, 6.0, Color::rgba(0, 0, 0, (150.0 * alpha) as u8));
        ui.label(pill.inset_xy(6.0, 0.0), &label, style);
    }

    fn draw_grid(&mut self, ui: &mut Ui, viewport: Rect, alpha: f32, dy: f32) {
        // Pull the next page before the loaded rows run out, so scrolling
        // never stalls at a page boundary. The first frame has no layout yet
        // and therefore no height to measure against; the second loads.
        if self.items.len() < self.timeline_total
            && self.grid.height > 0.0
            && self.scroll_target + viewport.h * 1.5 > self.grid.height
        {
            self.load_more_timeline();
        }
        let width = (viewport.w - 14.0).max(80.0);
        let key = (width.round() as u32, self.display.len());
        if key != self.grid_key {
            // The trash is not browsed by capture date, so its previews carry
            // no date headers at all.
            self.grid = match self.page {
                Page::Trash => grid::build_flat(&self.display, width, TILE_TARGET, TILE_GAP),
                _ => grid::build(&self.display, width, TILE_TARGET, TILE_GAP),
            };
            self.grid_key = key;
        }
        self.scroll_max = (self.grid.height - viewport.h).max(0.0);
        self.scroll_target = self.scroll_target.clamp(0.0, self.scroll_max);
        self.scroll = self.scroll.clamp(0.0, self.scroll_max);
        if !self.overlay() && viewport.contains(self.input.mouse) {
            let delta = self.input.wheel + self.input.precise;
            if delta.abs() > 0.01 {
                self.scroll_target = (self.scroll_target - delta).clamp(0.0, self.scroll_max);
            }
        }

        ui.paint_push_clip(viewport);
        let mut hovered_now: Option<String> = None;
        // An album tile that was clicked opens its page once the grid is done.
        let mut open_album: Option<String> = None;
        let over_scrollbar = self.scroll_max > 0.5 && self.input.mouse.0 > viewport.max().0 - 14.0;
        let over_sticky = self.input.mouse.1 < viewport.y + HEADER_H;
        // Tiles are in content space; the scroll and the transition offset are
        // added here. The cull band is 40 px beyond the viewport, so the tiles
        // it covers are one contiguous run, found by search.
        let offset = dy - self.scroll;
        self.prune_thumb_queue(viewport);
        let run = grid::tiles_between(&self.grid.tiles, -40.0 - offset, viewport.h + 40.0 - offset);
        // Every tile in view wants a thumbnail, so the cache holds at least twice
        // the visible run; it never shrinks, so scrolling does not evict them.
        self.thumbs
            .ensure_capacity((run.len() * 2).max(THUMB_CACHE));
        for tile_index in run {
            let (item_index, tile_rect) = {
                let tile = &self.grid.tiles[tile_index];
                (tile.index, tile.rect)
            };
            let rect = Rect::new(
                viewport.x + tile_rect.x,
                viewport.y + tile_rect.y + offset,
                tile_rect.w,
                tile_rect.h,
            );
            let item = match self.display.get(item_index).cloned() {
                Some(item) => item,
                None => continue,
            };
            let id = item.id.clone();
            let kind = item.kind;
            let thumb_state = item.thumb_state;
            let avg_color = item.avg_color;
            // An album tile keeps its first song's cover, but shows the whole
            // album's run time and name, and it is not a single track to select.
            let album = self.media_album(&item).cloned();
            let album_tile = album.is_some();
            let duration = match &album {
                Some(album) => Some(album.duration_ms),
                None => item.duration_ms,
            };
            let deleted_at = item.deleted_at;
            let selected = self.selection.contains(&id);
            let hover_here = self.hovered(rect);
            if hover_here {
                hovered_now = Some(id.clone());
                self.cursor_hand = true;
            }
            let h = self.hover_anim.get(&id).copied().unwrap_or(0.0);
            let tint_a = (255.0 * alpha).round() as u8;
            let key_tex = TexKind::Thumb.key(&id);
            let texture = self.thumbs.get(&key_tex);
            let has_art = texture.is_some();
            if let Some(texture) = texture {
                let uv = cover_uv(&texture, rect);
                ui.draw_list_mut().image_uv_rect(
                    rect,
                    &texture,
                    Color::WHITE.with_alpha(tint_a),
                    TILE_RADIUS,
                    uv,
                );
            } else {
                let base = if avg_color == 0 {
                    self.theme.g2
                } else {
                    Color::hex(avg_color)
                };
                ui.paint_round_rect(rect, TILE_RADIUS, fade(base, alpha));
                if thumb_state == ThumbState::Pending {
                    let pulse = 0.04 + 0.04 * ((self.time * 2.4).sin() * 0.5 + 0.5);
                    ui.paint_round_rect(
                        rect,
                        TILE_RADIUS,
                        Color::rgba(255, 255, 255, (pulse * 255.0 * alpha) as u8),
                    );
                } else if thumb_state == ThumbState::Failed && kind != Kind::Audio {
                    // An audio track's failed cover is drawn by the audio branch
                    // below as its music glyph; a second glyph here would overlap it.
                    paint_icon(
                        ui,
                        center_in(rect, (32.0, 32.0)),
                        AppIcon::Photos,
                        fade(self.theme.muted, alpha),
                    );
                }
            }
            self.request_thumb(&item);
            if h > 0.005 {
                ui.paint_round_rect(rect, TILE_RADIUS, fade(hover_overlay(self.dark, h), alpha));
            }
            if selected {
                ui.paint_round_ring(
                    rect.inset(1.0),
                    TILE_RADIUS,
                    2.0,
                    fade(self.theme.fg, alpha),
                );
            }
            // The dot fades on the tint's curve, so both appear and leave
            // together; selection holds it up once the pointer is gone.
            if !album_tile && (self.select_mode || selected || hover_here) {
                self.check_anim.entry(id.clone()).or_insert(0.0);
            }
            let dot = self.check_anim.get(&id).copied().unwrap_or(0.0) * alpha;
            if dot > 0.004 {
                let check = Rect::new(rect.x + 10.0, rect.y + 10.0, 24.0, 24.0);
                if selected {
                    ui.paint_round_rect(check, 12.0, fade(self.theme.fg, dot));
                } else {
                    ui.paint_round_rect(check, 12.0, Color::rgba(0, 0, 0, (110.0 * dot) as u8));
                    ui.paint_round_rect(
                        check.inset(1.0),
                        11.0,
                        Color::rgba(255, 255, 255, (220.0 * dot) as u8),
                    );
                    ui.paint_round_rect(
                        check.inset(2.0),
                        10.0,
                        Color::rgba(0, 0, 0, (90.0 * dot) as u8),
                    );
                }
                if selected {
                    paint_icon(
                        ui,
                        check.inset(5.0),
                        AppIcon::Check,
                        fade(self.theme.accent_text, dot),
                    );
                }
            }
            if kind == Kind::Video {
                let play = center_in(rect, (48.0, 48.0));
                if h > 0.004 {
                    ui.paint_round_rect(
                        play,
                        24.0,
                        Color::rgba(0, 0, 0, (120.0 * alpha * h) as u8),
                    );
                    paint_grid(
                        ui,
                        Rect::new(play.x + 12.5, play.y + 12.0, 24.0, 24.0),
                        24.0,
                        AppIcon::Play,
                        true,
                        fade(Color::WHITE, alpha * h),
                    );
                }
                if let Some(ms) = duration {
                    self.duration_pill(ui, rect, ms, alpha, tint_a);
                }
            }
            if kind == Kind::Audio {
                // A track with no cover is drawn as a music glyph and its title,
                // so the tile still says what it is. A cover needs no help.
                if !has_art {
                    let title = match &album {
                        Some(album) => album.name.clone(),
                        None => self
                            .track_tags
                            .get(&id)
                            .and_then(|audio| audio.tags.title.clone())
                            .unwrap_or_else(|| item.original_name.clone()),
                    };
                    paint_icon(
                        ui,
                        center_in(rect, (40.0, 40.0)),
                        AppIcon::Music,
                        fade(self.theme.muted, alpha),
                    );
                    self.label(
                        ui,
                        Rect::new(rect.x + 12.0, rect.max().1 - 40.0, rect.w - 24.0, 24.0),
                        &title,
                        13.0,
                        FontWeight::Medium,
                        fade(self.theme.fg, alpha),
                    );
                }
                if let Some(ms) = duration {
                    self.duration_pill(ui, rect, ms, alpha, tint_a);
                }
            }
            // Trashed items say how long they have before the sweep.
            if self.page == Page::Trash
                && let Some(deleted) = deleted_at
            {
                let label = archive_core::trash_days_label(deleted);
                let style = self.text_style(
                    ui,
                    11.0,
                    FontWeight::Medium,
                    Color::WHITE.with_alpha(tint_a),
                );
                let (text_width, _) = ui.measure(&label, style.font, 11.0);
                let countdown =
                    Rect::new(rect.x + 8.0, rect.max().1 - 28.0, text_width + 14.0, 20.0);
                ui.paint_round_rect(countdown, 6.0, Color::rgba(0, 0, 0, (150.0 * alpha) as u8));
                ui.label(countdown.inset_xy(7.0, 0.0), &label, style);
            }

            // A right press opens the tile's menu. An open menu does not stop
            // it: a right-click on another tile moves the menu there.
            if self.input.secondary_pressed
                && rect.contains(self.input.mouse)
                && !over_scrollbar
                && !over_sticky
                && !album_tile
                && self.modal.is_none()
                && self.viewer.is_none()
            {
                self.input.secondary_pressed = false;
                let rows = self.tile_menu(&id);
                self.open_menu(rows);
            }
            if !self.overlay() && !over_scrollbar && !over_sticky {
                // The dot is live as soon as the tile is hovered, so it can be
                // clicked even while it is still fading in.
                let check_rect = Rect::new(rect.x + 8.0, rect.y + 8.0, 28.0, 28.0);
                let dot_live = !album_tile && (self.select_mode || selected || hover_here);
                if self.input.pressed && check_rect.contains(self.input.mouse) && dot_live {
                    self.input.pressed = false;
                    self.toggle_selection(&id, item_index);
                } else if self.input.pressed && rect.contains(self.input.mouse) {
                    self.input.pressed = false;
                    if !album_tile && (self.select_mode || self.input.ctrl || self.input.shift) {
                        if self.input.shift {
                            self.select_range(item_index);
                        } else {
                            self.toggle_selection(&id, item_index);
                        }
                    } else {
                        self.press = Some(Press {
                            id: id.clone(),
                            index: item_index,
                            at: self.input.mouse,
                        });
                    }
                }
                if self.input.released && self.press.as_ref().is_some_and(|press| press.id == id) {
                    let press = self.press.take().expect("press was checked");
                    let moved = (self.input.mouse.0 - press.at.0).abs()
                        + (self.input.mouse.1 - press.at.1).abs();
                    if moved < 6.0 {
                        if let Some(album) = album {
                            open_album = Some(album.key);
                            break;
                        }
                        self.open_viewer(press.index);
                    }
                }
            }
        }

        // Date headers. The pinned one is drawn last so it sits above tiles.
        let sticky = self
            .grid
            .sticky_header(self.scroll)
            .map(|(group, _)| group.label.clone());
        for group in &self.grid.groups {
            let y = viewport.y + group.top - self.scroll + dy;
            if y + HEADER_H < viewport.y || y > viewport.max().1 {
                continue;
            }
            let header = Rect::new(viewport.x, y, viewport.w, HEADER_H);
            ui.rect(header, self.theme.bg);
            self.label(
                ui,
                Rect::new(header.x + 2.0, header.y, header.w - 4.0, header.h),
                &group.label,
                14.0,
                FontWeight::Medium,
                fade(self.theme.fg, alpha),
            );
        }
        if let Some(label) = sticky {
            let header = Rect::new(viewport.x, viewport.y, viewport.w, HEADER_H);
            ui.rect(header, self.theme.bg);
            self.label(
                ui,
                Rect::new(header.x + 2.0, header.y, header.w - 4.0, header.h),
                &label,
                14.0,
                FontWeight::Medium,
                fade(self.theme.fg, alpha),
            );
        }
        ui.paint_pop_clip();
        if hovered_now.is_some() {
            self.hover = hovered_now;
        } else if self
            .hover
            .as_deref()
            .is_some_and(|id| self.display.iter().any(|item| item.id == id))
        {
            self.hover = None;
        }
        self.draw_scrollbar(ui, viewport, alpha);
        if let Some(key) = open_album {
            self.set_page(Page::Album(key));
        }
    }

    fn toggle_selection(&mut self, id: &str, index: usize) {
        self.select_mode = true;
        if !self.selection.remove(id) {
            self.selection.insert(id.to_string());
        }
        self.sync_select_mode();
        if !self.selection.is_empty() {
            self.last_selected = Some(index);
        }
        self.dirty = true;
    }

    /// Selection mode is implied by having a selection: with nothing selected
    /// there is nothing to act on, so the bar goes away with the last tile.
    fn sync_select_mode(&mut self) {
        if self.selection.is_empty() {
            self.end_selection();
        }
    }

    /// Leaving selection mode forgets the shift-click anchor, so a new
    /// selection ranges from its own first tile rather than from one picked
    /// in a session that is already over.
    fn end_selection(&mut self) {
        self.select_mode = false;
        self.selection.clear();
        self.last_selected = None;
    }

    fn select_range(&mut self, index: usize) {
        // Shift-clicking an already selected tile deselects it on its own, exactly
        // like ctrl-clicking it would.
        let clicked = self.display[index].id.clone();
        if self.selection.contains(&clicked) {
            self.toggle_selection(&clicked, index);
            return;
        }
        let Some(start) = self.last_selected else {
            self.toggle_selection(&self.display[index].id.clone(), index);
            return;
        };
        self.select_mode = true;
        let (from, to) = if start <= index {
            (start, index)
        } else {
            (index, start)
        };
        for item in &self.display[from..=to.min(self.display.len() - 1)] {
            self.selection.insert(item.id.clone());
        }
        self.sync_select_mode();
        self.last_selected = Some(index);
        self.dirty = true;
    }

    fn draw_scrollbar(&mut self, ui: &mut Ui, viewport: Rect, alpha: f32) {
        if self.scroll_max <= 0.5 {
            self.scroll_drag = None;
            return;
        }
        let track = Rect::new(viewport.max().0 - 5.0, viewport.y, 4.0, viewport.h);
        ui.paint_round_rect(track, 2.0, fade(self.theme.g2, alpha));
        let content_h = self.grid.height.max(1.0);
        let thumb_h = (viewport.h * viewport.h / content_h).clamp(28.0, viewport.h);
        let travel = (viewport.h - thumb_h).max(1.0);
        let y = viewport.y + (self.scroll / self.scroll_max) * travel;
        let thumb = Rect::new(track.x, y, track.w, thumb_h);
        let active = self.scroll_drag.is_some()
            || (self.input.down && thumb.expand(8.0).contains(self.input.mouse));
        ui.paint_round_rect(
            thumb,
            2.0,
            fade(if active { self.theme.fg } else { self.theme.g3 }, alpha),
        );
        if !self.overlay() {
            if self.input.pressed && thumb.expand(10.0).contains(self.input.mouse) {
                self.scroll_drag = Some((self.input.mouse.1, self.scroll_target));
            } else if self.input.pressed && track.expand(8.0).contains(self.input.mouse) {
                let fraction =
                    ((self.input.mouse.1 - viewport.y - thumb_h * 0.5) / travel).clamp(0.0, 1.0);
                self.scroll_target = fraction * self.scroll_max;
                self.scroll_drag = Some((self.input.mouse.1, self.scroll_target));
            } else if let Some((start_y, start_scroll)) = self.scroll_drag {
                if self.input.down {
                    let delta = self.input.mouse.1 - start_y;
                    self.scroll_target = (start_scroll + delta / travel * self.scroll_max)
                        .clamp(0.0, self.scroll_max);
                } else {
                    self.scroll_drag = None;
                }
            }
        }
        if !self.input.down {
            self.scroll_drag = None;
        }
    }

    // --- Albums and collections -------------------------------------------

    /// The Albums and Collections pages: a grid of cover stacks, one per album.
    /// They differ only in which tiles they list and what a blank page says.
    fn draw_cover_grid(&mut self, ui: &mut Ui, content: Rect, alpha: f32, dy: f32) {
        let top = content.y + 10.0 + dy;
        let viewport = Rect::new(
            content.x + 24.0,
            top,
            (content.w - 48.0).max(20.0),
            (content.max().1 - top - 14.0).max(0.0),
        );
        let music = self.page == Page::Albums;
        let has_any = if music {
            !self.albums.is_empty()
        } else {
            !self.collections.is_empty()
        };
        if !has_any || viewport.h < 20.0 {
            if !has_any {
                self.scroll = 0.0;
                self.scroll_target = 0.0;
                self.scroll_max = 0.0;
                // Like every other blank page: one grey icon, one grey line
                // and a hint. Collections are born in the selection bar rather
                // than from a button here; albums come from imported music.
                let (icon, title, hint) = if music {
                    (
                        AppIcon::Music,
                        "No albums yet",
                        "Imported music with an album tag is sorted into albums here",
                    )
                } else {
                    (
                        AppIcon::Collections,
                        "No collections yet",
                        "Select photos or videos, then add them to a collection",
                    )
                };
                let center = center_in(viewport, (460.0, 250.0));
                let top = center.y + ((center.h - 96.0) * 0.5).max(0.0);
                paint_icon(
                    ui,
                    Rect::new(center.center().0 - 26.0, top, 52.0, 52.0),
                    icon,
                    fade(self.theme.faint, alpha),
                );
                self.label_center(
                    ui,
                    Rect::new(center.x, top + 64.0, center.w, 32.0),
                    title,
                    21.0,
                    FontWeight::SemiBold,
                    fade(self.theme.faint, alpha),
                );
                self.label_center(
                    ui,
                    Rect::new(center.x, top + 100.0, center.w, 24.0),
                    hint,
                    13.0,
                    FontWeight::Regular,
                    fade(self.theme.muted, alpha),
                );
            }
            return;
        }
        if self.cover_tiles.is_empty() {
            self.scroll = 0.0;
            self.scroll_target = 0.0;
            self.scroll_max = 0.0;
            self.draw_empty(ui, viewport, alpha);
            return;
        }

        let gap = 14.0;
        let cols = (((viewport.w + gap) / 214.0).floor() as usize).clamp(2, 6);
        let card_w = ((viewport.w - gap * (cols - 1) as f32) / cols as f32).max(120.0);
        let cover_h = card_w;
        let card_h = cover_h + 50.0;
        let rows = self.cover_tiles.len().div_ceil(cols);
        let total = rows as f32 * (card_h + gap);
        self.scroll_max = (total - viewport.h).max(0.0);
        self.scroll_target = self.scroll_target.clamp(0.0, self.scroll_max);
        self.scroll = self.scroll.clamp(0.0, self.scroll_max);
        if !self.overlay() && viewport.contains(self.input.mouse) {
            let delta = self.input.wheel + self.input.precise;
            if delta.abs() > 0.01 {
                self.scroll_target = (self.scroll_target - delta).clamp(0.0, self.scroll_max);
            }
        }

        ui.paint_push_clip(viewport);
        // Cards sit on a regular grid, so the rows that can be on screen are
        // computed rather than found by testing every card. The per-card test
        // below stays as the exact cut-off.
        let step = card_h + gap;
        let first_row = ((self.scroll - dy - 20.0 - card_h) / step).floor().max(0.0) as usize;
        let last_row = ((self.scroll - dy + viewport.h + 20.0) / step)
            .ceil()
            .max(0.0) as usize;
        let count = self.cover_tiles.len();
        for index in (first_row * cols).min(count)..((last_row + 1) * cols).min(count) {
            let tile = self.cover_tiles[index].clone();
            let row = index / cols;
            let col = index % cols;
            let card = Rect::new(
                viewport.x + col as f32 * (card_w + gap),
                viewport.y + row as f32 * (card_h + gap) - self.scroll + dy,
                card_w,
                card_h,
            );
            if card.max().1 < viewport.y - 20.0 || card.y > viewport.max().1 + 20.0 {
                continue;
            }
            let cover = Rect::new(card.x, card.y, card.w, cover_h);
            let hovered = self.hovered(cover);
            let layers = self
                .covers
                .get(&tile.cover_key)
                .cloned()
                .unwrap_or_default();
            let mut washes = Vec::new();
            if hovered {
                self.cursor_hand = true;
                washes.push(fade(hover_overlay(self.dark, 1.0), alpha));
            }
            // A locked collection shows its lock. An album is one cover. Collections are
            // stacks of their newest images.
            if tile.lock == LockState::Locked {
                self.draw_locked_cover(ui, cover, alpha, &washes);
            } else if matches!(tile.page, Page::Album(_)) {
                self.draw_cover_single(ui, cover, 8.0, layers.first(), alpha, &washes);
            } else {
                self.draw_cover_stack(ui, cover, 8.0, &layers, alpha, &washes);
            }
            self.label_ellipsis(
                ui,
                Rect::new(card.x, cover.max().1 + 6.0, card.w, 22.0),
                &tile.name,
                13.5,
                FontWeight::Medium,
                fade(self.theme.fg, alpha),
            );
            self.label(
                ui,
                Rect::new(card.x, cover.max().1 + 26.0, card.w, 18.0),
                &tile.count_label,
                12.0,
                FontWeight::Regular,
                fade(self.theme.muted, alpha),
            );
            let over_scrollbar =
                self.scroll_max > 0.5 && self.input.mouse.0 > viewport.max().0 - 14.0;
            if self.input.secondary_pressed
                && card.contains(self.input.mouse)
                && !over_scrollbar
                && self.modal.is_none()
                && self.viewer.is_none()
            {
                self.input.secondary_pressed = false;
                self.open_menu(Self::collection_menu(&tile.page, tile.lock));
            }
            if !over_scrollbar && self.take_press(cover) {
                ui.paint_pop_clip();
                self.set_page(tile.page.clone());
                return;
            }
        }
        ui.paint_pop_clip();
        self.draw_scrollbar(ui, viewport, alpha);
    }

    // --- Viewer ----------------------------------------------------------

    /// The media column: the space left of the Details panel, between the
    /// name line above and the action pill below.
    pub fn viewer_area(width: f32, height: f32, panel_width: f32) -> Rect {
        Rect::new(
            80.0,
            92.0,
            (width - 160.0 - panel_width).max(120.0),
            (height - 184.0).max(120.0),
        )
    }

    /// Physical pixels of the media column with the Details panel shut. That
    /// is the largest the column can be, so a player decodes at least as much
    /// as the viewer will ever show.
    fn media_pixels(&self) -> (u32, u32) {
        let area = Self::viewer_area(self.screen.0, self.screen.1, 0.0);
        (
            (area.w * self.scale).round().max(1.0) as u32,
            (area.h * self.scale).round().max(1.0) as u32,
        )
    }

    /// Called from a player's thread when it has something new. The closure
    /// pokes the event loop so the next frame polls the player.
    fn wake(&self) -> Wake {
        let proxy = self.proxy.clone();
        Arc::new(move || {
            if let Some(proxy) = &proxy {
                let _ = proxy.send_event(());
            }
        })
    }

    /// Choose the engine for `item` and start it. Players return at once and
    /// load on their own threads.
    fn open_media(&self, item: &Item, now: Instant) -> MediaView {
        let path = self.library.source_path(item);
        let wake = self.wake();
        let (width, height) = self.media_pixels();
        let playback = match item.kind {
            // Music plays through the same engine as video, with no picture
            // stream. The viewer draws the cover in place of a frame.
            Kind::Video | Kind::Audio => {
                let mut player = VideoPlayer::open(path, width, height, wake, true);
                player.set_volume(self.video_volume);
                player.set_muted(self.video_muted);
                Playback::Video(Box::new(player))
            }
            Kind::Image if is_animated_ext(&item.ext) => {
                let side = width.max(height).min(ANIM_MAX_SIDE);
                Playback::Anim(Box::new(AnimPlayer::open(
                    path,
                    side,
                    ANIM_GPU_BUDGET,
                    wake,
                )))
            }
            _ => Playback::Still,
        };
        MediaView::new(item.id.clone(), playback, now)
    }

    /// Start the media for the open item and reset what belonged to the one
    /// before it.
    fn start_media(&mut self, id: &str) {
        let now = Instant::now();
        let media = self.item_by_id(id).map(|item| {
            let mut media = self.open_media(&item, now);
            media.lyrics = self.lyrics_of(&item);
            media
        });
        self.set_viewer_media(media);
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.reset_for_item(now);
        }
    }

    /// The synced lyrics of a song, parsed for the player. Empty for anything
    /// that is not a song, or a song imported without an `.lrc`.
    fn lyrics_of(&self, item: &Item) -> Vec<LyricLine> {
        if item.kind != Kind::Audio {
            return Vec::new();
        }
        self.library
            .lyrics(&item.id)
            .ok()
            .flatten()
            .map(|text| parse_lyrics(&text))
            .unwrap_or_default()
    }

    /// The synced lyrics of the open item, in time order.
    fn open_lyrics(&self) -> &[LyricLine] {
        match self
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.media.as_ref())
        {
            Some(media) => media.lyrics.as_slice(),
            None => &[],
        }
    }

    /// When the next synced line starts, while the song plays. The line on
    /// screen only changes on a redraw, and a still picture or the audio poll
    /// would otherwise let it run late, so the viewer wakes for it directly.
    fn next_lyric_wake(media: &MediaView, now: Instant) -> Option<Instant> {
        let Playback::Video(player) = &media.playback else {
            return None;
        };
        if !matches!(player.status(), PlayerStatus::Playing) {
            return None;
        }
        let lines = media.lyrics.as_slice();
        let position = player.position_ms(now);
        let next = current_lyric(lines, position).map_or(0, |index| index + 1);
        let line = lines.get(next)?;
        let wait = (line.at_ms - position).max(0) as u64;
        Some(now + std::time::Duration::from_millis(wait))
    }

    /// The lyric on screen, centred in the band above the video bar. Only the
    /// current line is shown, with a black outline so it reads over the cover.
    /// Nothing is drawn before the first line starts or in an instrumental gap.
    fn draw_lyrics(&self, ui: &mut Ui, band: Rect, position_ms: i64, open: f32) {
        let lines = self.open_lyrics();
        let Some(index) = current_lyric(lines, position_ms) else {
            return;
        };
        let text = lines[index].text.as_str();
        if text.is_empty() {
            return;
        }
        // Dream draws no text outline, so the line is drawn in black at a ring
        // of offsets, and the white line goes over the top.
        for (dx, dy) in LYRIC_OUTLINE {
            let rect = Rect::new(
                band.x + dx * LYRIC_OUTLINE_PX,
                band.y + dy * LYRIC_OUTLINE_PX,
                band.w,
                band.h,
            );
            let style = self
                .text_style(
                    ui,
                    LYRIC_SIZE,
                    FontWeight::Medium,
                    Color::BLACK.with_alpha((255.0 * open) as u8),
                )
                .align(Align::Center)
                .vcenter(true);
            ui.label_ellipsis(rect, text, style);
        }
        let style = self
            .text_style(
                ui,
                LYRIC_SIZE,
                FontWeight::Medium,
                Color::WHITE.with_alpha((255.0 * open) as u8),
            )
            .align(Align::Center)
            .vcenter(true);
        ui.label_ellipsis(band, text, style);
    }

    /// Swap the viewer's media. The old player stops as it drops, and its
    /// textures go to the retired list rather than being freed now.
    fn set_viewer_media(&mut self, media: Option<MediaView>) {
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        let old = std::mem::replace(&mut viewer.media, media);
        if let Some(old) = old {
            self.viewer_retired.extend(old.into_retired());
        }
    }

    /// Close the viewer. Its players stop as they drop, and its textures wait
    /// out the frames still in flight.
    pub fn close_viewer(&mut self) {
        let Some(viewer) = self.viewer.take() else {
            return;
        };
        if let Some(media) = viewer.media {
            self.viewer_retired.extend(media.into_retired());
        }
    }

    /// The open video, if the viewer is showing one.
    fn video_mut(&mut self) -> Option<&mut VideoPlayer> {
        self.viewer.as_mut()?.media.as_mut()?.video_mut()
    }

    /// Pull what the players have ready and upload it. Runs once per rendered
    /// frame, before drawing, so a frame announced by a player's thread shows
    /// up in the frame that follows the wake-up.
    fn advance_viewer(&mut self, now: Instant) {
        let Some(context) = self.context.clone() else {
            return;
        };
        let Some(media) = self
            .viewer
            .as_mut()
            .and_then(|viewer| viewer.media.as_mut())
        else {
            return;
        };
        let falls_back = match &media.playback {
            Playback::Anim(player) => matches!(
                player.status(),
                AnimStatus::NotAnimated | AnimStatus::Failed(_)
            ),
            _ => false,
        };
        if falls_back {
            // A file that is not really animated, or whose animation failed,
            // shows as a still. The failure is not worth a message of its own:
            // the preview is still there.
            media.playback = Playback::Still;
            media.clear_cache();
            media.clear_frame();
            return;
        }

        let mut uploaded: Option<(Arc<GpuTexture>, usize, bool)> = None;
        let mut reuse: Option<usize> = None;
        let mut streaming = false;
        match &mut media.playback {
            Playback::Anim(player) => {
                if let Some(tick) = player.poll(now) {
                    match tick.frame {
                        Some(frame) => {
                            uploaded = upload_frame(&context, "archive animation frame", &frame)
                                .map(|texture| (texture, tick.index, tick.keep));
                            player.recycle(frame.rgba);
                        }
                        None => reuse = Some(tick.index),
                    }
                    streaming = !tick.keep;
                }
            }
            Playback::Video(player) => {
                if let Some(frame) = player.poll(now) {
                    uploaded = upload_frame(&context, "archive video frame", &frame)
                        .map(|texture| (texture, 0, false));
                    player.recycle(frame.rgba);
                }
            }
            Playback::Still => {}
        }

        // A streaming animation keeps nothing by index, so each loop starts
        // from an empty cache and any old entries are retired.
        if streaming {
            media.clear_cache();
        }
        if let Some((texture, index, keep)) = uploaded {
            if keep {
                media.set_cached(index, Arc::clone(&texture));
            }
            media.set_frame(texture);
        }
        if let Some(index) = reuse
            && let Some(Some(texture)) = media.anim_cache.get(index).cloned()
        {
            media.set_frame(texture);
        }
    }

    /// Once per frame, let the retired textures count down, and let the
    /// viewer's own media do the same for what it has replaced.
    fn retire_viewer_textures(&mut self) {
        for (_, frames) in self.viewer_retired.iter_mut() {
            *frames = frames.saturating_sub(1);
        }
        self.viewer_retired.retain(|(_, frames)| *frames > 0);
        if let Some(media) = self
            .viewer
            .as_mut()
            .and_then(|viewer| viewer.media.as_mut())
        {
            media.retire_frame();
        }
    }

    /// Ask for the full-resolution decode of the open still. It waits until
    /// the preview is up and the still has been on screen for a moment, and
    /// only one decode is outstanding at a time. A failure is remembered so
    /// the same file is not decoded again every frame.
    fn request_full(&mut self, item: &Item, now: Instant) {
        if let Some((_, asked)) = &self.full_pending {
            if asked.elapsed() < FULL_DECODE_TIMEOUT {
                return;
            }
            self.full_pending = None;
        }
        if self.full_failed.contains(&item.id) {
            return;
        }
        if item.kind != Kind::Image {
            return;
        }
        let Some(native) = item.width.zip(item.height).map(|(w, h)| w.max(h)) else {
            return;
        };
        let Some(media) = self
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.media.as_ref())
        else {
            return;
        };
        if media.id != item.id
            || media.full.is_some()
            || media.kind() != MediaKind::Still
            || now < media.full_wanted_at
        {
            return;
        }
        let Some(preview) = self.previews.get(&TexKind::Preview.key(&item.id)) else {
            return;
        };
        let (preview_w, preview_h) = preview.size();
        let preview_long = preview_w.max(preview_h);
        // A preview that hit the proxy cap was cut down from something larger,
        // even when the catalogue says otherwise: libraries imported before
        // tiled HEIF was probed correctly recorded one 512 px tile.
        if native <= preview_long && preview_long < archive_core::VIEWER_SIDE {
            return;
        }
        let Some(pool) = self.texture_pool.as_ref() else {
            return;
        };
        self.full_pending = Some((item.id.clone(), Instant::now()));
        pool.request(TextureJob {
            key: TexKind::Full.key(&item.id),
            kind: TexKind::Full,
            item: item.clone(),
            max_side: archive_core::FULL_SIDE,
            epoch: self.vault_epoch,
        });
    }

    /// The result of a full decode. It is uploaded only if it still belongs to
    /// the open item; otherwise it is dropped. Full textures stay out of the
    /// preview LRU, because they are large and only the open item needs them.
    fn on_full_texture(&mut self, id: &str, result: Result<(u32, u32, Vec<u8>), String>) {
        if self
            .full_pending
            .as_ref()
            .is_some_and(|(pending, _)| pending == id)
        {
            self.full_pending = None;
        }
        let Ok((width, height, pixels)) = result else {
            self.full_failed.insert(id.to_string());
            return;
        };
        let Some(context) = self.context.clone() else {
            return;
        };
        let Some(media) = self
            .viewer
            .as_mut()
            .and_then(|viewer| viewer.media.as_mut())
        else {
            return;
        };
        if media.id != id {
            return;
        }
        // The full mip chain: a full still is drawn well below its native
        // size at fit zoom, and without mips it would shimmer.
        if let Some(texture) =
            thumbs::upload(&context, "archive full", width, height, &pixels, None)
        {
            media.set_full(Arc::new(texture));
        }
    }

    /// Start or stop the open video, or step the open animation. A video that
    /// has ended starts again from the beginning.
    fn toggle_playback(&mut self) {
        let now = Instant::now();
        let Some(media) = self
            .viewer
            .as_mut()
            .and_then(|viewer| viewer.media.as_mut())
        else {
            return;
        };
        match &mut media.playback {
            Playback::Video(player) => {
                if matches!(player.status(), PlayerStatus::Ended) {
                    player.play(now);
                } else {
                    player.toggle(now);
                }
            }
            Playback::Anim(player) => player.toggle(now),
            Playback::Still => {}
        }
    }

    fn toggle_mute(&mut self) {
        if let Some(player) = self.video_mut() {
            let muted = !player.muted();
            player.set_muted(muted);
            self.video_muted = muted;
            self.hold_volume_slider();
        }
    }

    /// Bring the volume slider out for a moment, so a change made without the
    /// slider (a key) is seen in it.
    fn hold_volume_slider(&mut self) {
        if let Some(viewer) = self.viewer.as_mut() {
            let now = Instant::now();
            viewer.volume_hold_until = now + VOLUME_HOLD;
            viewer.controls_until = now + CONTROLS_IDLE;
        }
    }

    fn viewer_volume(&mut self, delta: f32) {
        if let Some(player) = self.video_mut() {
            let volume = (player.volume() + delta).clamp(0.0, 1.0);
            player.set_volume(volume);
            self.video_volume = volume;
            self.hold_volume_slider();
        }
    }

    fn viewer_seek(&mut self, delta_ms: i64) {
        let now = Instant::now();
        if let Some(player) = self.video_mut() {
            let target = player.position_ms(now) + delta_ms;
            player.seek(target, now);
        }
    }

    /// Frames the open video or animation has put on screen. The rendercheck
    /// reads this to see playback move between two captures.
    pub fn viewer_frames_shown(&self) -> u64 {
        self.viewer
            .as_ref()
            .and_then(|viewer| viewer.media.as_ref())
            .map_or(0, |media| media.frames_shown)
    }

    /// The open still's zoom as drawn, and its target.
    pub fn viewer_zoom(&self) -> (f32, f32) {
        self.viewer
            .as_ref()
            .map_or((1.0, 1.0), |viewer| (viewer.view.zoom, viewer.view.target))
    }

    /// Whether the open still has its full-resolution decode on screen.
    pub fn viewer_full_loaded(&self) -> bool {
        self.viewer
            .as_ref()
            .and_then(|viewer| viewer.media.as_ref())
            .is_some_and(|media| media.full.is_some())
    }

    /// Whether the viewer needs a frame on its own account: something is
    /// moving, held or fading. Playback does not count, since the players
    /// wake the loop themselves.
    fn viewer_animating(&self) -> bool {
        let Some(viewer) = self.viewer.as_ref() else {
            return false;
        };
        let open_target = if viewer.closing { 0.0 } else { 1.0 };
        let info_target = if viewer.info_open { 1.0 } else { 0.0 };
        let loading = viewer.media.as_ref().is_some_and(|media| {
            media.frame.is_none()
                && media
                    .video()
                    .is_some_and(|player| matches!(player.status(), PlayerStatus::Loading))
        });
        (viewer.open - open_target).abs() > 0.002
            || !viewer.info.settled(info_target)
            || viewer.drag.is_some()
            || viewer.scrubbing
            || viewer.volume_drag
            || viewer.chrome_busy
            || (viewer.view.zoom - viewer.view.target).abs() > 0.001
            || (viewer.controls - viewer.controls_goal).abs() > 0.002
            || loading
    }

    /// When the viewer next needs a frame without anything else happening:
    /// the earliest of the players' own schedules, the full decode's
    /// debounce, and the video bar's auto-hide.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        [self.vault_wake(), self.viewer_wake(now), self.caret_wake()]
            .into_iter()
            .flatten()
            .min()
    }

    fn viewer_wake(&self, now: Instant) -> Option<Instant> {
        let viewer = self.viewer.as_ref()?;
        let mut soonest: Option<Instant> = None;
        let mut consider = |at: Instant| {
            soonest = Some(soonest.map_or(at, |current| current.min(at)));
        };
        if let Some(media) = viewer.media.as_ref() {
            let player = match &media.playback {
                Playback::Anim(player) => player.next_wake(now),
                Playback::Video(player) => player.next_wake(now),
                Playback::Still => None,
            };
            if let Some(at) = player {
                consider(at);
            }
            if let Some(at) = Self::next_lyric_wake(media, now) {
                consider(at);
            }
            if media.kind() == MediaKind::Still
                && media.full.is_none()
                && self.full_pending.is_none()
                && now < media.full_wanted_at
            {
                consider(media.full_wanted_at);
            }
        }
        if viewer.controls_goal > 0.5 && viewer.controls_until > now {
            consider(viewer.controls_until);
        }
        if viewer.volume_hold_until > now {
            consider(viewer.volume_hold_until);
        }
        soonest
    }

    /// Ease the video bar toward its wanted visibility. The wanted state is
    /// recorded here so that [`Self::viewer_animating`] can see the fade.
    fn step_viewer_controls(&mut self, dt: f32) {
        let now = self.last_frame;
        let mouse = self.input.mouse;
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        let shown = viewer
            .media
            .as_ref()
            .and_then(MediaView::video)
            .is_some_and(|player| {
                controls_wanted(
                    &player.status(),
                    now,
                    viewer.controls_until,
                    viewer.bar.contains(mouse),
                )
            });
        let goal = if shown { 1.0 } else { 0.0 };
        viewer.controls_goal = goal;
        let step = dt / CONTROLS_FADE;
        viewer.controls = if viewer.controls < goal {
            (viewer.controls + step).min(goal)
        } else {
            (viewer.controls - step).max(goal)
        };

        // The controls' own motion. The icons and the slider ease toward what
        // the player says, so a key press animates them as a drag would.
        let (playing, volume, muted) = viewer.media.as_ref().and_then(MediaView::video).map_or(
            (false, 1.0, false),
            |player| {
                (
                    matches!(player.status(), PlayerStatus::Playing),
                    player.volume(),
                    player.muted(),
                )
            },
        );
        let linear = |value: f32, goal: f32, seconds: f32| {
            let step = dt / seconds;
            if value < goal {
                (value + step).min(goal)
            } else {
                (value - step).max(goal)
            }
        };
        let mut busy = false;
        let open_goal = if goal > 0.5
            && (viewer.volume_drag
                || now < viewer.volume_hold_until
                || viewer.volume_zone.contains(mouse))
        {
            1.0
        } else {
            0.0
        };
        viewer.volume_open = linear(viewer.volume_open, open_goal, 0.18);
        busy |= viewer.volume_open != open_goal;

        let play_goal = if playing { 1.0 } else { 0.0 };
        viewer.play_shown = linear(viewer.play_shown, play_goal, 0.16);
        busy |= viewer.play_shown != play_goal;

        let silent = muted || volume <= 0.0;
        let marks = if silent {
            [0.0, 0.0, 1.0]
        } else {
            [1.0, if volume >= 0.5 { 1.0 } else { 0.0 }, 0.0]
        };
        for (shown, goal) in viewer.speaker_shown.iter_mut().zip(marks) {
            *shown = linear(*shown, goal, 0.16);
            busy |= *shown != goal;
        }

        // A drag writes the spring directly while it holds the pointer.
        if !viewer.volume_drag {
            let target = if silent { 0.0 } else { volume };
            viewer.volume_shown.step(target, dt, 22.0);
            busy |= !viewer.volume_shown.settled(target);
        }
        viewer.chrome_busy = busy;
    }

    /// What the viewer draws from its media in one frame, copied out so the
    /// drawing can borrow the app again.
    fn media_state(&self) -> MediaState {
        let now = Instant::now();
        let Some(media) = self
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.media.as_ref())
        else {
            return MediaState::default();
        };
        let mut state = MediaState {
            kind: media.kind(),
            frame: media.frame.clone(),
            full: media.full.clone(),
            ..MediaState::default()
        };
        if let Some(player) = media.video() {
            let status = player.status();
            state.playing = matches!(status, PlayerStatus::Playing);
            state.status = Some(status);
            // Music has no picture stream, so it has no display size of its
            // own. The cover's size is used instead, as for a still.
            state.audio_only = player.info().is_some_and(|info| !info.has_video);
            state.display = player
                .info()
                .filter(|info| info.has_video)
                .map(|info| (info.width, info.height));
            state.position_ms = player.position_ms(now);
            state.duration_ms = player.duration_ms();
            state.muted = player.muted();
            state.volume = player.volume();
        }
        state
    }

    /// The pixel-for-pixel zoom of the open still, and the largest zoom the
    /// viewer allows. `native_long` is the still's longest edge in pixels.
    fn zoom_limits(&self, area: Rect, aspect: f32, native_long: f32) -> (f32, f32) {
        let fit = fit_in(area, aspect).size();
        let actual = actual_zoom(native_long, fit, self.scale);
        (actual, max_zoom(actual))
    }

    /// The video controls: play, the progress track, and the volume icon with
    /// its slider. Returns true when a press landed on them. `active` is false
    /// while they are fading out, so invisible controls take no presses.
    fn draw_video_bar(
        &mut self,
        ui: &mut Ui,
        layout: &BarLayout,
        state: &MediaState,
        alpha: f32,
        active: bool,
    ) -> bool {
        let fg = self.theme.fg;
        let mouse = self.input.mouse;
        let down = self.input.down;
        let (mut scrubbing, mut volume_drag, open, play_shown, speaker, mut volume_spring) =
            self.viewer.as_ref().map_or(
                (false, false, 0.0, 0.0, [0.0; 3], Spring::at(1.0)),
                |viewer| {
                    (
                        viewer.scrubbing,
                        viewer.volume_drag,
                        ease_out_cubic(viewer.volume_open),
                        ease_out_cubic(viewer.play_shown),
                        viewer.speaker_shown.map(ease_out_cubic),
                        viewer.volume_shown,
                    )
                },
            );
        let duration = state.duration_ms.unwrap_or(0);
        let mut handled = false;

        // Play and pause trade places in the same spot. The button also
        // replays a video that has ended.
        let hovered = active && layout.play.contains(mouse);
        if hovered {
            self.cursor_hand = true;
        }
        let ink = fade(fg, alpha * if hovered { 1.0 } else { 0.85 });
        bar_glyph(
            ui,
            layout.play,
            AppIcon::Play,
            1.0 - play_shown,
            (0.5, 0.5),
            ink,
        );
        bar_glyph(ui, layout.play, AppIcon::Pause, play_shown, (0.5, 0.5), ink);
        if hovered && self.input.pressed {
            self.input.pressed = false;
            self.dirty = true;
            self.toggle_playback();
            handled = true;
        }

        // Scrubbing: a press on the track starts it, and it lasts while the
        // button is held. Each frame seeks to the pointer, which the player
        // coalesces, so a fast drag does not start a decoder per move.
        if active && self.input.pressed && layout.track.contains(mouse) {
            self.input.pressed = false;
            self.dirty = true;
            handled = true;
            scrubbing = true;
        }
        if scrubbing {
            if down {
                let target = (fraction_along(layout.track, mouse.0) * duration as f32) as i64;
                let now = Instant::now();
                if let Some(player) = self.video_mut() {
                    player.seek(target, now);
                }
            } else {
                scrubbing = false;
            }
        }
        let fraction = if scrubbing {
            fraction_along(layout.track, mouse.0)
        } else if duration > 0 {
            (state.position_ms as f32 / duration as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let hovered_track = active && layout.track.contains(mouse);
        self.bar_track(
            ui,
            layout.track,
            fraction,
            alpha,
            hovered_track || scrubbing,
        );

        // The speaker is one body with marks layered on the same 24 grid, so
        // every variant sits alike and the marks can fade in and out.
        let hovered_mute = active && layout.mute.contains(mouse);
        if hovered_mute {
            self.cursor_hand = true;
        }
        let ink = fade(fg, alpha * if hovered_mute { 1.0 } else { 0.85 });
        let anchor = (11.0 / 24.0, 0.5);
        bar_glyph(ui, layout.mute, AppIcon::VolumeBody, 1.0, anchor, ink);
        bar_glyph(
            ui,
            layout.mute,
            AppIcon::VolumeWaveLow,
            speaker[0],
            anchor,
            ink,
        );
        bar_glyph(
            ui,
            layout.mute,
            AppIcon::VolumeWaveHigh,
            speaker[1],
            anchor,
            ink,
        );
        bar_glyph(
            ui,
            layout.mute,
            AppIcon::VolumeCross,
            speaker[2],
            (19.0 / 24.0, 0.5),
            ink,
        );
        if hovered_mute && self.input.pressed {
            self.input.pressed = false;
            self.dirty = true;
            self.toggle_mute();
            handled = true;
        }

        // The slider reveals rightward from the icon. It follows the pointer
        // while its button is held; any other change eases in the spring.
        let slider_live = active && open > 0.9;
        if slider_live && self.input.pressed && layout.volume.contains(mouse) && !hovered_mute {
            self.input.pressed = false;
            self.dirty = true;
            handled = true;
            volume_drag = true;
        }
        if volume_drag {
            if down {
                let volume = fraction_along(layout.volume_track, mouse.0);
                if let Some(player) = self.video_mut() {
                    player.set_volume(volume);
                    if volume > 0.0 {
                        player.set_muted(false);
                    }
                }
                self.video_volume = volume;
                volume_spring = Spring::at(volume);
            } else {
                volume_drag = false;
            }
        }
        if open > 0.01 {
            // The bar's right edge is the wipe: the slider is drawn at its
            // place and only what has come inside the edge shows.
            let left = layout.volume_track.x - 8.0;
            let right = layout.bar.x + layout.bar.w + 8.0;
            ui.paint_push_clip(Rect::new(
                left,
                layout.volume_track.y,
                (right - left).max(0.0),
                layout.volume_track.h,
            ));
            let hovered_volume = slider_live && layout.volume.contains(mouse);
            self.bar_track(
                ui,
                layout.volume_track,
                volume_spring.x,
                alpha * open,
                hovered_volume || volume_drag,
            );
            ui.paint_pop_clip();
        }

        if let Some(viewer) = self.viewer.as_mut() {
            viewer.scrubbing = scrubbing;
            viewer.volume_drag = volume_drag;
            if volume_drag {
                viewer.volume_shown = volume_spring;
            }
        }
        handled
    }

    /// A slim track with its filled part and, when wanted, a knob at the fill.
    fn bar_track(&self, ui: &mut Ui, track: Rect, fraction: f32, alpha: f32, knob: bool) {
        let fraction = fraction.clamp(0.0, 1.0);
        let y = track.y + track.h * 0.5 - 2.0;
        // The outline is a black rect grown on every side, drawn first so the
        // track and fill sit on top of it.
        #[cfg(not(feature = "plain-controls"))]
        ui.paint_round_rect(
            Rect::new(
                track.x - BAR_OUTLINE,
                y - BAR_OUTLINE,
                track.w + 2.0 * BAR_OUTLINE,
                4.0 + 2.0 * BAR_OUTLINE,
            ),
            2.0 + BAR_OUTLINE,
            fade(Color::rgba(0, 0, 0, 255), alpha * BAR_OUTLINE_ALPHA),
        );
        ui.paint_round_rect(
            Rect::new(track.x, y, track.w, 4.0),
            2.0,
            fade(self.theme.faint, alpha),
        );
        let filled = track.w * fraction;
        if filled > 0.5 {
            ui.paint_round_rect(
                Rect::new(track.x, y, filled, 4.0),
                2.0,
                fade(self.theme.fg, alpha),
            );
        }
        if knob {
            let x = track.x + filled;
            #[cfg(not(feature = "plain-controls"))]
            ui.paint_round_rect(
                Rect::new(
                    x - 6.0 - BAR_OUTLINE,
                    y - 4.0 - BAR_OUTLINE,
                    12.0 + 2.0 * BAR_OUTLINE,
                    12.0 + 2.0 * BAR_OUTLINE,
                ),
                6.0 + BAR_OUTLINE,
                fade(Color::rgba(0, 0, 0, 255), alpha * BAR_OUTLINE_ALPHA),
            );
            ui.paint_round_rect(
                Rect::new(x - 6.0, y - 4.0, 12.0, 12.0),
                6.0,
                fade(self.theme.fg, alpha),
            );
        }
    }

    fn draw_viewer(&mut self, ui: &mut Ui, width: f32, height: f32) {
        self.advance_music();
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        if viewer.open <= 0.002 {
            return;
        }
        let now = Instant::now();
        let open = ease_out_quint(viewer.open);
        let info = viewer.info.x.clamp(0.0, 1.0);
        let info_open = viewer.info_open;
        let index = viewer.index;
        let len = viewer.ids.len();
        let id = viewer.ids.get(index).cloned();
        let controls_alpha = viewer.controls;
        let volume_open = ease_out_cubic(viewer.volume_open);
        let mut view = viewer.view;
        let mut drag = viewer.drag;
        let last_click = viewer.last_click;
        let zoom_step = self
            .viewer
            .as_mut()
            .and_then(|viewer| viewer.zoom_step.take());
        let item = id.as_ref().and_then(|id| self.item_by_id(id));
        let state = self.media_state();
        // A right press anywhere in the viewer opens the menu for the picture.
        if self.input.secondary_pressed
            && self.modal.is_none()
            && self.viewer.as_ref().is_some_and(|viewer| !viewer.closing)
        {
            self.input.secondary_pressed = false;
            let rows = self.viewer_menu();
            self.open_menu(rows);
        }
        // Inert while a menu is up, fading or not, so a press that dismisses
        // the menu does not also reach the viewer's controls.
        let interactive = self.modal.is_none() && self.menu.is_none();
        let mouse = self.input.mouse;

        if let Some(item) = &item {
            self.request_thumb(item);
            self.request_preview(&item.id);
            self.request_full(item, now);
        }

        // The best still we have: the full decode, else the preview, else the
        // thumbnail. A video or animation draws its current frame over it.
        let preview = id
            .as_ref()
            .and_then(|id| self.previews.get(&TexKind::Preview.key(id)));
        let thumb = id
            .as_ref()
            .and_then(|id| self.thumbs.get(&TexKind::Thumb.key(id)));
        let drawn = state
            .frame
            .clone()
            .or(state.full.clone())
            .or(preview)
            .or(thumb);

        // Video uses the size the decoder reports, which includes rotation.
        // Everything else uses the drawn texture, which carries the EXIF turn
        // the decoder applied; the stored dimensions do not.
        let aspect = match (state.kind, state.display, &drawn) {
            (MediaKind::Video, Some((w, h)), _) => w.max(1) as f32 / h.max(1) as f32,
            (_, _, Some(texture)) => {
                let (w, h) = texture.size();
                w.max(1) as f32 / h.max(1) as f32
            }
            _ => item.as_ref().map_or(1.0, Item::aspect),
        };

        let panel_width = 344.0 * info;
        let full_area = Self::viewer_area(width, height, panel_width);
        // A song with lyrics keeps a band above the video bar for its subtitles,
        // and its cover is fitted above that band.
        // Decided from the lyrics, which are read before the first frame, not
        // from the player: its probe lags, and the layout must not jump later.
        let has_lyrics = !self.open_lyrics().is_empty();
        let bar_top = bar_layout(full_area, height, volume_open).bar.y;
        let lyric_band = Rect::new(
            full_area.x,
            bar_top - LYRIC_BAND,
            full_area.w,
            LYRIC_BAND - LYRIC_GAP,
        );
        let area = if has_lyrics {
            let bottom = lyric_band.y - LYRIC_GAP;
            Rect::new(
                full_area.x,
                full_area.y,
                full_area.w,
                (bottom - full_area.y).max(120.0),
            )
        } else {
            full_area
        };
        let zoomable = item.is_some() && matches!(state.kind, MediaKind::Still | MediaKind::Anim);
        let native_long = state
            .full
            .as_ref()
            .map(|texture| texture.size())
            .or_else(|| item.as_ref().and_then(|item| item.width.zip(item.height)))
            .map_or(0.0, |(w, h)| w.max(h) as f32);
        let (actual, max) = self.zoom_limits(area, aspect, native_long);
        let centre = (area.x + area.w * 0.5, area.y + area.h * 0.5);

        // Zoom and pan belong to stills and animations. Video stays at fit.
        if !zoomable {
            view = ViewTransform::default();
        } else {
            if let Some(step) = zoom_step {
                let target = match step {
                    ZoomStep::In => view.target * KEY_ZOOM_STEP,
                    ZoomStep::Out => view.target / KEY_ZOOM_STEP,
                    ZoomStep::Reset => 1.0,
                };
                view = zoom_at(view, area, aspect, clamp_zoom(target, max), centre);
            }
            let delta = self.input.wheel + self.input.precise;
            if interactive && delta != 0.0 && area.contains(mouse) {
                let target = clamp_zoom(view.target * (delta * WHEEL_ZOOM).exp(), max);
                view = zoom_at(view, area, aspect, target, mouse);
                self.input.wheel = 0.0;
                self.input.precise = 0.0;
            }
        }

        // A drag that is still held pans by how far the pointer moved since
        // it last reported. The press that starts a drag is handled below.
        if let Some(anchor) = drag {
            if self.input.down {
                let (dx, dy) = (mouse.0 - anchor.0, mouse.1 - anchor.1);
                if view.zoom > 1.0001 && (dx != 0.0 || dy != 0.0) {
                    view = pan(view, area, aspect, dx, dy);
                }
                drag = Some(mouse);
            } else {
                drag = None;
            }
        }
        view = ease_view(view, area, aspect, self.frame_dt);

        let rect = if item.is_some() {
            image_rect(area, aspect, &view)
        } else {
            center_in(area, (320.0, 240.0))
        };
        // A zoomed still takes presses over the whole column, since its picture
        // may run past the letterbox. Anything else takes presses only on the
        // picture itself.
        let media_hit = if zoomable && (view.zoom > 1.0001 || view.target > 1.0001) {
            area
        } else {
            rect
        };

        let scrim_alpha = (232.0 * open) as u8;
        ui.rect(
            Rect::new(0.0, 0.0, width, height),
            Color::rgba(8, 8, 10, scrim_alpha),
        );
        let mut handled = false;
        let mut pill_rect = Rect::default();
        let mut bar_rect = Rect::default();
        let mut volume_zone = Rect::default();

        // The picture is clipped to the viewport left of the Details panel,
        // so a zoomed still can run past the letterbox. Chrome is drawn over it.
        if let Some(texture) = &drawn {
            let viewport = Rect::new(0.0, 0.0, (width - panel_width).max(0.0), height);
            ui.paint_push_clip(viewport);
            ui.draw_list_mut().image_rounded(
                rect,
                texture,
                Color::WHITE.with_alpha((255.0 * open) as u8),
                0.0,
            );
            ui.paint_pop_clip();
        } else if let Some(item) = &item {
            ui.paint_round_rect(
                rect,
                8.0,
                fade(
                    if item.avg_color == 0 {
                        self.theme.g2
                    } else {
                        Color::hex(item.avg_color)
                    },
                    open,
                ),
            );
            paint_icon(
                ui,
                center_in(rect, (44.0, 44.0)),
                if item.kind == Kind::Audio {
                    AppIcon::Music
                } else {
                    AppIcon::Photos
                },
                Color::WHITE.with_alpha((160.0 * open) as u8),
            );
        }

        // Video states that replace the picture: a pulsing loader until the
        // first frame, and a message if the file cannot be played. The message
        // sits over the middle of the column, since the video bar and pill take
        // the space below the picture.
        if state.kind == MediaKind::Video && state.frame.is_none() {
            match &state.status {
                Some(PlayerStatus::Loading) => {
                    let pulse = 0.5 + 0.35 * (self.time * 3.0).sin();
                    paint_icon(
                        ui,
                        center_in(rect, (44.0, 44.0)),
                        AppIcon::LoaderCircle,
                        Color::WHITE.with_alpha((255.0 * open * pulse) as u8),
                    );
                }
                Some(PlayerStatus::Failed(reason)) => {
                    self.label_center(
                        ui,
                        Rect::new(area.x, centre.1 - 26.0, area.w, 22.0),
                        "This video can't be played",
                        13.0,
                        FontWeight::Medium,
                        Color::WHITE.with_alpha((220.0 * open) as u8),
                    );
                    self.label_center(
                        ui,
                        Rect::new(area.x, centre.1 - 2.0, area.w, 18.0),
                        reason,
                        11.5,
                        FontWeight::Regular,
                        Color::WHITE.with_alpha((150.0 * open) as u8),
                    );
                }
                _ => {}
            }
        }

        // Top chrome.
        if let Some(item) = &item {
            // The name and date sit above the image, centred on the same
            // column the image is letterboxed in.
            self.label_center(
                ui,
                Rect::new(area.x, 26.0, area.w, 22.0),
                &item.original_name,
                13.0,
                FontWeight::Medium,
                Color::WHITE.with_alpha((220.0 * open) as u8),
            );
            self.label_center(
                ui,
                Rect::new(area.x, 48.0, area.w, 18.0),
                &archive_core::format_datetime(item.captured_at),
                11.5,
                FontWeight::Regular,
                Color::WHITE.with_alpha((150.0 * open) as u8),
            );
            if let Some(deleted) = item.deleted_at {
                self.label_center(
                    ui,
                    Rect::new(area.x, 68.0, area.w, 18.0),
                    &archive_core::trash_days_label(deleted),
                    11.5,
                    FontWeight::Medium,
                    fade(self.theme.danger, open),
                );
            }
        }

        // Previous / next.
        let prev = Rect::new(18.0, height * 0.5 - 24.0, 48.0, 48.0);
        if index > 0 && self.viewer_icon_button(ui, prev, AppIcon::Back, false, interactive, open) {
            self.viewer_step(-1);
            handled = true;
        }
        let next = Rect::new(width - panel_width - 66.0, height * 0.5 - 24.0, 48.0, 48.0);
        if index + 1 < len
            && self.viewer_icon_button(ui, next, AppIcon::Forward, false, interactive, open)
        {
            self.viewer_step(1);
            handled = true;
        }

        // Synced lyrics, as subtitles in the band above the bar.
        if has_lyrics {
            self.draw_lyrics(ui, lyric_band, state.position_ms, open);
        }

        // The video bar. It sits above the pill and fades with its own state.
        if state.kind == MediaKind::Video && item.is_some() {
            let layout = bar_layout(area, height, volume_open);
            bar_rect = layout.bar;
            volume_zone = layout.volume;
            if controls_alpha > 0.01 {
                let active = interactive && controls_alpha > 0.5;
                if self.draw_video_bar(ui, &layout, &state, controls_alpha * open, active) {
                    handled = true;
                }
            }
        }

        // Presses on the picture: a click toggles a video, and a press on a
        // still or animation starts a pan, or zooms if it is a double click.
        let press_media =
            interactive && !handled && self.input.pressed && media_hit.contains(mouse);
        if press_media {
            self.input.pressed = false;
            handled = true;
            match state.kind {
                MediaKind::Video => self.toggle_playback(),
                MediaKind::Still | MediaKind::Anim => {
                    let double = is_double_click(last_click, now, mouse);
                    if double {
                        let target = if view.target > 1.01 {
                            1.0
                        } else {
                            clamp_zoom(actual.max(2.0), max)
                        };
                        view = zoom_at(view, area, aspect, target, mouse);
                    }
                    if let Some(viewer) = self.viewer.as_mut() {
                        viewer.last_click = if double { None } else { Some((now, mouse)) };
                    }
                    drag = Some(mouse);
                }
                MediaKind::None => {}
            }
        }

        // Action pill.
        let mut actions: Vec<(AppIcon, &str, bool)> = Vec::new();
        if item.is_some() {
            actions.push((AppIcon::Info, "Details", info_open));
            if self.page == Page::Trash {
                actions.push((AppIcon::Download, "Restore", false));
                // Trash items can go now instead of waiting out the retention.
                actions.push((AppIcon::Trash, "Delete", false));
            } else {
                if matches!(self.page, Page::Collection(_)) {
                    actions.push((AppIcon::CircleMinus, "Remove", false));
                }
                actions.push((AppIcon::Trash, "Trash", false));
            }
        }
        if let Some(item) = &item {
            let item = item.clone();
            let mut label_widths = Vec::with_capacity(actions.len());
            for (_, label, _) in &actions {
                label_widths.push(measure_text_width(ui, label, 12.0));
            }
            let pill_w = label_widths.iter().sum::<f32>() + 52.0 * actions.len() as f32 + 8.0;
            let pill = Rect::new(
                (width - panel_width - pill_w) * 0.5,
                height - 76.0,
                pill_w.min(width - 160.0),
                48.0,
            );
            pill_rect = pill;
            ui.paint_round_rect(pill, 24.0, fade(self.theme.g1, open));
            let mut bx = pill.x + 4.0;
            for (action_index, (icon, label, active)) in actions.into_iter().enumerate() {
                let button_w = label_widths[action_index] + 48.0;
                let button = Rect::new(bx, pill.y + 4.0, button_w, 40.0);
                // Restore leans green and the destructive actions lean red;
                // Details keeps the plain ink.
                let tint = match label {
                    "Restore" => self.theme.ok,
                    "Trash" | "Delete" => self.theme.danger,
                    _ => self.theme.fg,
                };
                if self.viewer_text_button(ui, button, icon, label, tint, active, interactive, open)
                {
                    match label {
                        "Details" => {
                            if let Some(viewer) = self.viewer.as_mut() {
                                viewer.info_open = !viewer.info_open;
                            }
                        }
                        "Remove" => {
                            if let Page::Collection(collection_id) = self.page {
                                match self.library.remove_from_collection(
                                    collection_id,
                                    std::slice::from_ref(&item.id),
                                ) {
                                    Ok(_) => {
                                        self.close_viewer();
                                        self.reload();
                                        self.toast("Removed 1 item from the collection");
                                    }
                                    Err(error) => self.toast(&error.to_string()),
                                }
                            }
                        }
                        "Trash" | "Delete" => self.ask_delete_viewer(),
                        "Restore" => match self.library.restore(std::slice::from_ref(&item.id)) {
                            Ok(_) => {
                                self.close_viewer();
                                self.reload();
                                self.toast("Item restored from the trash");
                            }
                            Err(error) => self.toast(&error.to_string()),
                        },
                        _ => {}
                    }
                    handled = true;
                }
                bx += button_w + 2.0;
            }
        }

        // Details panel. It slides in from the right edge at its fixed width.
        // It also fades with the viewer, so it leaves with the rest of the chrome.
        if info > 0.002 {
            let panel_x = width + 8.0 - 360.0 * info;
            let panel = Rect::new(panel_x, 8.0, 344.0, height - 16.0);
            if self.draw_details(ui, panel, info * open, item.as_ref(), interactive) {
                handled = true;
            }
        }

        // A press anywhere off the picture, its bar and its pill closes the view.
        if !handled && interactive && self.input.pressed {
            let on_bar = controls_alpha > 0.5 && bar_rect.contains(mouse);
            if media_hit.contains(mouse) || pill_rect.contains(mouse) || on_bar {
                self.input.pressed = false;
            } else {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.closing = true;
                }
                self.input.pressed = false;
            }
        }

        if let Some(viewer) = self.viewer.as_mut() {
            viewer.view = view;
            viewer.drag = drag;
            viewer.bar = bar_rect;
            viewer.volume_zone = volume_zone;
        }
    }

    /// The Details panel: summary blocks that drop down to their facts. `info`
    /// is how far the panel is shown, and sets its opacity. Each block opens on
    /// its own spring. The list scrolls with the wheel. Returns true when the
    /// press landed here.
    fn draw_details(
        &mut self,
        ui: &mut Ui,
        panel: Rect,
        info: f32,
        item: Option<&Item>,
        interactive: bool,
    ) -> bool {
        let mut handled = false;
        ui.paint_round_rect(panel, 14.0, fade(self.theme.g1, info));
        ui.paint_push_clip(panel);
        let inner_x = panel.x + 20.0;

        let audio = item.and_then(|item| self.track_meta(item));
        let blocks = item
            .map(|item| details::blocks(item, audio.as_ref()))
            .unwrap_or_default();
        // The blocks start at the top of the panel now that the name is above the image.
        let list = Rect::new(panel.x, panel.y + 20.0, panel.w, (panel.h - 32.0).max(0.0));
        let total: f32 = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                let (_, body) = details::place(&block.fields, DETAILS_BODY_W);
                let open = self.details_spring[index].x.clamp(0.0, 1.0);
                CARD_HEAD_H + drop_height(body) * open + CARD_GAP
            })
            .sum();
        let max_scroll = (total - list.h).max(0.0);
        if interactive && list.contains(self.input.mouse) {
            let delta = self.input.wheel + self.input.precise;
            if delta.abs() > 0.01 {
                self.details_scroll -= delta;
            }
        }
        self.details_scroll = self.details_scroll.clamp(0.0, max_scroll);

        ui.paint_push_clip(list);
        let mut y = list.y - self.details_scroll;
        for (index, block) in blocks.iter().enumerate() {
            let open = self.details_spring[index].x.clamp(0.0, 1.0);
            let expandable = block.available && !block.fields.is_empty();
            // A block with nothing for this file is greyed out, not explained.
            let alpha = info * if block.available { 1.0 } else { 0.35 };
            let (placed, body) = details::place(&block.fields, DETAILS_BODY_W);
            let card = Rect::new(
                inner_x,
                y,
                DETAILS_INNER_W,
                CARD_HEAD_H + drop_height(body) * open,
            );
            ui.paint_round_rect(card, 14.0, fade(self.theme.g2, info));

            paint_icon(
                ui,
                Rect::new(card.x + 14.0, card.y + 16.0, 20.0, 20.0),
                block.icon,
                fade(self.theme.fg, alpha),
            );
            let text_x = card.x + 46.0;
            let text_w = (card.max().0 - 40.0 - text_x).max(40.0);
            // With no summary line, the title centres on the icon instead.
            let title_y = if block.summary.is_empty() { 17.0 } else { 8.0 };
            self.label_ellipsis(
                ui,
                Rect::new(text_x, card.y + title_y, text_w, 18.0),
                &block.title,
                14.0,
                FontWeight::Medium,
                fade(self.theme.fg, alpha),
            );
            if !block.summary.is_empty() {
                self.label_ellipsis(
                    ui,
                    Rect::new(text_x, card.y + 27.0, text_w, 16.0),
                    &block.summary,
                    12.0,
                    FontWeight::Regular,
                    fade(self.theme.muted, alpha),
                );
            }

            let head = Rect::new(card.x, card.y, card.w, CARD_HEAD_H);
            if expandable {
                let hovered = interactive
                    && list.contains(self.input.mouse)
                    && head.contains(self.input.mouse);
                let chevron = Rect::new(card.max().0 - 30.0, card.y + 17.0, 18.0, 18.0);
                let ink = if hovered {
                    self.theme.fg
                } else {
                    self.theme.muted
                };
                // The two chevrons cross-fade, so the arrow seems to turn over.
                paint_icon(
                    ui,
                    chevron,
                    AppIcon::ChevronDown,
                    fade(ink, info * (1.0 - open)),
                );
                paint_icon(ui, chevron, AppIcon::ChevronUp, fade(ink, info * open));
                if hovered {
                    self.cursor_hand = true;
                    if self.input.pressed {
                        self.input.pressed = false;
                        self.details_open[index] = !self.details_open[index];
                        self.dirty = true;
                        handled = true;
                    }
                }
            }

            if expandable && open > 0.001 {
                let body_clip = Rect::new(
                    card.x,
                    card.y + CARD_HEAD_H,
                    card.w,
                    drop_height(body) * open,
                );
                // The body rides down into place as it opens, and fades in.
                let slide = (1.0 - open) * -10.0;
                let body_x = card.x + 12.0;
                let body_y = card.y + CARD_HEAD_H + 6.0 + slide;
                ui.paint_push_clip(body_clip);
                for cell in &placed {
                    let field = &block.fields[cell.field];
                    let x = body_x + cell.x;
                    let y = body_y + cell.y;
                    self.label(
                        ui,
                        Rect::new(x, y, cell.w, 16.0),
                        field.label,
                        11.0,
                        FontWeight::Medium,
                        fade(self.theme.muted, open * info),
                    );
                    self.label_ellipsis(
                        ui,
                        Rect::new(x, y + 17.0, cell.w, 20.0),
                        &field.value,
                        13.0,
                        FontWeight::Regular,
                        fade(self.theme.fg, open * info),
                    );
                }
                ui.paint_pop_clip();
            }
            y += card.h + CARD_GAP;
        }
        ui.paint_pop_clip();
        ui.paint_pop_clip();

        if self.input.pressed
            && panel.contains(self.input.mouse)
            && self.modal.is_none()
            && self.menu.is_none()
        {
            self.input.pressed = false;
            handled = true;
        }
        handled
    }

    fn viewer_icon_button(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        icon: AppIcon,
        active: bool,
        interactive: bool,
        alpha: f32,
    ) -> bool {
        let hovered = interactive && rect.contains(self.input.mouse);
        let bg = if active {
            Color::WHITE.with_alpha((52.0 * alpha) as u8)
        } else if hovered {
            Color::WHITE.with_alpha((34.0 * alpha) as u8)
        } else {
            Color::WHITE.with_alpha((18.0 * alpha) as u8)
        };
        ui.paint_round_rect(rect, rect.w * 0.5, bg);
        paint_icon(
            ui,
            rect.inset(rect.w * 0.24),
            icon,
            Color::WHITE.with_alpha((230.0 * alpha) as u8),
        );
        if hovered {
            self.cursor_hand = true;
        }
        if interactive && self.input.pressed && rect.contains(self.input.mouse) {
            self.input.pressed = false;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn viewer_text_button(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        icon: AppIcon,
        label: &str,
        tint: Color,
        active: bool,
        interactive: bool,
        alpha: f32,
    ) -> bool {
        let hovered = interactive && rect.contains(self.input.mouse);
        // No container: the icon and label themselves carry the state. The
        // ink brightens on hover and sits at full strength while selected.
        let ink = fade(
            tint,
            alpha
                * if active {
                    1.0
                } else if hovered {
                    0.85
                } else {
                    0.62
                },
        );
        paint_icon(
            ui,
            Rect::new(rect.x + 10.0, rect.y + 11.0, 18.0, 18.0),
            icon,
            ink,
        );
        self.label(
            ui,
            Rect::new(rect.x + 34.0, rect.y, rect.w - 40.0, rect.h),
            label,
            12.5,
            FontWeight::Medium,
            ink,
        );
        if hovered {
            self.cursor_hand = true;
        }
        if interactive && self.input.pressed && rect.contains(self.input.mouse) {
            self.input.pressed = false;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    // --- Modal -----------------------------------------------------------

    fn on_remote_import_changed(&mut self) {
        let session = match &self.modal {
            Some(Modal::RemoteImport(session) | Modal::RemoteImportDetails(session)) => {
                session.clone()
            }
            _ => return,
        };
        match session.status() {
            crate::remote_import::Status::Ready(paths) => {
                self.modal = None;
                self.import_staged(paths, Some(session.staging.clone()));
            }
            crate::remote_import::Status::Closed(reason) => {
                self.modal = None;
                self.toast(&reason);
            }
            _ => self.dirty = true,
        }
    }

    fn draw_import_tile(&mut self, ui: &mut Ui, rect: Rect, icon: AppIcon, label: &str) -> bool {
        let hovered = self.modal_hover(rect);
        ui.paint_round_rect(
            rect,
            16.0,
            if hovered {
                self.theme.g3
            } else {
                self.theme.g2
            },
        );
        if hovered {
            self.cursor_hand = true;
        }
        paint_grid(
            ui,
            Rect::new(rect.x + (rect.w - 64.0) * 0.5, rect.y + 50.0, 64.0, 64.0),
            80.0,
            icon,
            false,
            self.theme.faint,
        );
        self.label_center(
            ui,
            Rect::new(rect.x + 8.0, rect.y + 138.0, rect.w - 16.0, 24.0),
            label,
            14.0,
            FontWeight::Medium,
            self.theme.faint,
        );
        self.modal_take_press(rect)
    }

    fn draw_import_source(&mut self, ui: &mut Ui, width: f32, height: f32, t: f32) {
        let remote = match self.modal.clone() {
            Some(Modal::RemoteImport(session)) => Some((session, false)),
            Some(Modal::RemoteImportDetails(session)) => Some((session, true)),
            _ => None,
        };
        let details = remote.as_ref().is_some_and(|(_, details)| *details);
        let card_w = if remote.is_some() && !details {
            352.0_f32
        } else {
            440.0_f32
        }
        .min(width - 32.0);
        let card_h = if details {
            220.0
        } else if remote.is_some() {
            370.0
        } else {
            320.0
        };
        let card = Rect::new(
            (width - card_w) * 0.5,
            (height - card_h) * 0.5 + (1.0 - t) * 10.0,
            card_w,
            card_h,
        );
        ui.paint_round_rect(card, 16.0, self.theme.g1);
        ui.paint_round_ring(card, 16.0, 1.25, fade(self.modal_outline(), t));
        self.label_center(
            ui,
            Rect::new(card.x, card.y + 22.0, card.w, 26.0),
            if details {
                "Connection details"
            } else if remote.is_some() {
                "Scan from your device"
            } else {
                "File Import Method"
            },
            17.0,
            FontWeight::SemiBold,
            self.theme.muted,
        );
        if let Some((session, details)) = remote {
            if details {
                let style = self
                    .text_style(ui, 12.0, FontWeight::Regular, self.theme.muted)
                    .align(Align::Center);
                ui.label_wrapped(
                    Rect::new(card.x + 24.0, card.y + 60.0, card.w - 48.0, 38.0),
                    "Verify the browser certificate's SHA-256 fingerprint before uploading:",
                    style,
                );
                for (i, line) in session.fingerprint.as_bytes().chunks(48).enumerate() {
                    self.label_center(
                        ui,
                        Rect::new(
                            card.x + 16.0,
                            card.y + 104.0 + i as f32 * 20.0,
                            card.w - 32.0,
                            20.0,
                        ),
                        std::str::from_utf8(line).unwrap(),
                        11.5,
                        FontWeight::Medium,
                        self.theme.fg,
                    );
                }
                if self.modal_button(
                    ui,
                    Rect::new(card.x + (card.w - 120.0) * 0.5, card.y + 168.0, 120.0, 34.0),
                    "Back",
                    ModalTone::Plain,
                    true,
                ) {
                    self.modal = Some(Modal::RemoteImport(session));
                }
            } else {
                // Integer QR modules with a four-module white quiet zone.
                let n = session.qr.width();
                let cell = (248.0 / (n + 8) as f32).floor().max(1.0);
                let side = (n + 8) as f32 * cell;
                let x = (card.x + (card.w - side) * 0.5).round();
                let y = (card.y + 64.0 + (248.0 - side) * 0.5).round();
                ui.rect(Rect::new(x, y, side, side), Color::WHITE);
                for row in 0..n {
                    for col in 0..n {
                        if session.qr[(col, row)] == qrcode::Color::Dark {
                            ui.rect(
                                Rect::new(
                                    x + (col + 4) as f32 * cell,
                                    y + (row + 4) as f32 * cell,
                                    cell,
                                    cell,
                                ),
                                Color::rgb(0, 0, 0),
                            );
                        }
                    }
                }
                let link = Rect::new(card.x + 24.0, card.y + 326.0, card.w - 48.0, 22.0);
                self.label_center(
                    ui,
                    link,
                    &format!("https://{}", session.address),
                    12.0,
                    FontWeight::Regular,
                    self.theme.muted,
                );
                if self.modal_hover(link) {
                    self.cursor_hand = true;
                }
                if self.modal_take_press(link) {
                    self.modal = Some(Modal::RemoteImportDetails(session));
                }
            }
        } else {
            let size = 184.0;
            let left = Rect::new(
                card.x + (card.w - (size * 2.0 + 16.0)) * 0.5,
                card.y + 80.0,
                size,
                size,
            );
            let right = Rect::new(left.x + size + 16.0, left.y, size, size);
            if self.draw_import_tile(ui, left, AppIcon::FolderOpen, "Local Files") {
                self.modal = None;
                self.pick_files();
            }
            if self.draw_import_tile(ui, right, AppIcon::Smartphone, "Wireless Transfer") {
                let proxy = self.proxy.clone();
                let tx = self.events_tx.clone();
                match crate::remote_import::Session::start(move || {
                    let _ = tx.send(AppEvent::RemoteImportChanged);
                    if let Some(proxy) = &proxy {
                        let _ = proxy.send_event(());
                    }
                }) {
                    Ok(session) => self.modal = Some(Modal::RemoteImport(session)),
                    Err(error) => self.toast(&error),
                }
            }
        }
        if self.input.pressed && !card.contains(self.input.mouse) {
            self.input.pressed = false;
            self.modal = None;
        }
    }

    fn draw_modal(&mut self, ui: &mut Ui, width: f32, height: f32) {
        if self.modal.is_none() {
            return;
        }
        // A short eased entrance keeps the surface and its outline arriving
        // together. Both dialogs share the scrim.
        let t = ease_out_cubic(self.modal_t);
        ui.rect(
            Rect::new(0.0, 0.0, width, height),
            Color::rgba(0, 0, 0, (120.0 * t) as u8),
        );
        match self.modal {
            Some(Modal::ImportSource | Modal::RemoteImport(_) | Modal::RemoteImportDetails(_)) => {
                self.draw_import_source(ui, width, height, t)
            }
            Some(Modal::AddToCollection { .. }) => self.draw_collection_sheet(ui, width, height, t),
            Some(Modal::Import(_)) => self.draw_import_sheet(ui, width, height, t),
            Some(Modal::Password(_)) => self.draw_password(ui, width, height, t),
            _ => self.draw_confirm(ui, width, height, t),
        }
        // A dismissal during drawing has already painted the modal this frame.
        if self.modal.is_none() {
            self.dirty = true;
        }
    }

    fn modal_outline(&self) -> Color {
        if self.dark {
            Color::rgb(0x54, 0x55, 0x5a)
        } else {
            Color::rgb(0xb8, 0xbc, 0xc2)
        }
    }

    /// The confirm dialog: a centred card with a title, an optional line of
    /// body text, and a cancel and confirm pair. It hugs its content.
    fn draw_confirm(&mut self, ui: &mut Ui, width: f32, height: f32, t: f32) {
        let Some(Modal::Confirm {
            title,
            body,
            confirm,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        let card_h = if body.is_empty() { 128.0 } else { 168.0 };
        // Both buttons share one width, sized to the longer label, and the
        // pair is centred under the centred title.
        let btn_w = (measure_text_width(ui, &confirm, 13.5) + 40.0).max(120.0);
        let content = (measure_text_width(ui, &title, 17.0) * 1.1).max(btn_w * 2.0 + 8.0);
        let card_w = (content + 64.0).clamp(300.0, 420.0);
        let card = Rect::new(
            (width - card_w) * 0.5,
            (height - card_h) * 0.5 + (1.0 - t) * 10.0,
            card_w,
            card_h,
        );
        ui.paint_round_rect(card, 16.0, self.theme.g1);
        ui.paint_round_ring(card, 16.0, 1.25, fade(self.modal_outline(), t));
        let y = card.y + 22.0;
        self.label_center(
            ui,
            Rect::new(card.x, y, card.w, 24.0),
            &title,
            17.0,
            FontWeight::SemiBold,
            fade(self.theme.fg, t),
        );
        if !body.is_empty() {
            let style = self
                .text_style(ui, 13.0, FontWeight::Regular, fade(self.theme.muted, t))
                .line_height(1.3)
                .align(Align::Center);
            let body_rect = Rect::new(card.x + 24.0, y + 30.0, card.w - 48.0, 34.0);
            ui.label_wrapped(body_rect, &body, style);
        }
        let pair_x = card.x + (card.w - (btn_w * 2.0 + 8.0)) * 0.5;
        let cancel = Rect::new(pair_x, card.max().1 - 58.0, btn_w, 38.0);
        let confirm_rect = Rect::new(cancel.max().0 + 8.0, cancel.y, btn_w, 38.0);
        if self.modal_button(ui, cancel, "Cancel", ModalTone::Plain, true) {
            self.modal = None;
        }
        if self.modal_button(ui, confirm_rect, &confirm, ModalTone::Danger, true) {
            self.confirm_modal();
        }
        if self.modal.is_some() && self.input.pressed && !card.contains(self.input.mouse) {
            self.input.pressed = false;
            self.modal = None;
        }
    }

    /// The password dialog: a centred card with a field for the password, and a
    /// second for its confirmation when a lock is being made. Its buttons are the
    /// same pair as the other dialogs'.
    fn draw_password(&mut self, ui: &mut Ui, width: f32, height: f32, t: f32) {
        let Some(Modal::Password(session)) = self.modal.clone() else {
            return;
        };
        let create = session.mode == PasswordMode::Create;
        let card_w = 380.0_f32.min(width - 32.0);
        let card_h = if create { 272.0 } else { 222.0 };
        let card = Rect::new(
            (width - card_w) * 0.5,
            (height - card_h) * 0.5 + (1.0 - t) * 10.0,
            card_w,
            card_h,
        );
        ui.paint_round_rect(card, 16.0, self.theme.g1);
        ui.paint_round_ring(card, 16.0, 1.25, fade(self.modal_outline(), t));

        let (title, body, confirm) = match session.mode {
            PasswordMode::Create => (
                "Lock this collection",
                "Choose a password. Nothing can recover it, so keep it somewhere safe.",
                "Lock",
            ),
            PasswordMode::Unlock => (
                "Unlock this collection",
                "Enter the password to see what is inside.",
                "Unlock",
            ),
            PasswordMode::RemoveLock => (
                "Remove the lock",
                "Enter the password. Its photos and videos return to the library.",
                "Remove lock",
            ),
        };
        self.label_center(
            ui,
            Rect::new(card.x, card.y + 22.0, card.w, 24.0),
            title,
            17.0,
            FontWeight::SemiBold,
            fade(self.theme.fg, t),
        );
        let style = self
            .text_style(ui, 13.0, FontWeight::Regular, fade(self.theme.muted, t))
            .line_height(1.3)
            .align(Align::Center);
        ui.label_wrapped(
            Rect::new(card.x + 28.0, card.y + 52.0, card.w - 56.0, 36.0),
            body,
            style,
        );

        let field_w = card.w - 56.0;
        let mut rows = vec![(
            PasswordField::Password,
            "Password",
            session.password.chars().count(),
        )];
        if create {
            rows.push((
                PasswordField::Confirm,
                "Confirm password",
                session.confirm.chars().count(),
            ));
        }
        let mut y = card.y + 98.0;
        for (field, placeholder, typed) in rows {
            let rect = Rect::new(card.x + 28.0, y, field_w, 38.0);
            let focused = session.field == field;
            ui.paint_round_rect(rect, 10.0, fade(self.theme.g2, t));
            let outline = if focused {
                self.theme.fg
            } else {
                self.modal_outline()
            };
            ui.paint_round_ring(rect, 10.0, 1.25, fade(outline, t));
            // The typed text is shown as dots, never as characters.
            let (text, color) = if typed == 0 {
                (placeholder.to_string(), self.theme.muted)
            } else {
                ("•".repeat(typed), self.theme.fg)
            };
            self.label(
                ui,
                Rect::new(rect.x + 12.0, rect.y, rect.w - 24.0, rect.h),
                &text,
                15.0,
                FontWeight::Regular,
                fade(color, t),
            );
            if !session.busy && self.input.pressed && rect.contains(self.input.mouse) {
                self.input.pressed = false;
                if let Some(Modal::Password(current)) = self.modal.as_mut() {
                    current.field = field;
                }
            }
            y += 48.0;
        }

        let status = if session.busy {
            Some((
                "Working on the lock. A large collection takes a moment.",
                self.theme.muted,
            ))
        } else {
            session
                .error
                .as_deref()
                .map(|error| (error, self.theme.danger))
        };
        if let Some((text, color)) = status {
            self.label_center(
                ui,
                Rect::new(card.x + 28.0, y - 6.0, field_w, 18.0),
                text,
                12.5,
                FontWeight::Regular,
                fade(color, t),
            );
        }

        let btn_w = 124.0;
        let pair_x = card.x + (card.w - (btn_w * 2.0 + 8.0)) * 0.5;
        let cancel = Rect::new(pair_x, card.max().1 - 58.0, btn_w, 38.0);
        let confirm_rect = Rect::new(cancel.max().0 + 8.0, cancel.y, btn_w, 38.0);
        let confirm = if session.busy { "Working…" } else { confirm };
        if self.modal_button(ui, cancel, "Cancel", ModalTone::Plain, !session.busy) {
            self.modal = None;
        }
        if self.modal_button(ui, confirm_rect, confirm, ModalTone::Primary, !session.busy) {
            self.submit_password();
        }
        // Clicking away dismisses it, like the other dialogs, except while a lock
        // is being worked on: that work cannot be called off from here.
        if self.modal.is_some()
            && !session.busy
            && self.input.pressed
            && !card.contains(self.input.mouse)
        {
            self.input.pressed = false;
            self.modal = None;
        }
    }

    /// Paints a collection's cover as a stack of sheets. The newest image is
    /// the front sheet and the older ones sit behind it, peeking out above and
    /// to the right. A layer with no image yet is a plain grey placeholder.
    /// Each wash is painted over every sheet in turn, so it covers exactly the
    /// visible shape of the stack rather than its bounding box.
    fn draw_cover_stack(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        radius: f32,
        layers: &[Item],
        alpha: f32,
        washes: &[Color],
    ) {
        // The front sheet is inset from the bottom left, which leaves room for
        // the sheets behind it to show along its top and right edges.
        let step = rect.w * COVER_STEP;
        let front = Rect::new(
            rect.x,
            rect.y + step * 2.0,
            rect.w - step * 2.0,
            rect.h - step * 2.0,
        );
        // Back to front, so each sheet is painted over the ones behind it.
        for layer in (0..COVER_LAYERS).rev() {
            let offset = step * layer as f32;
            let sheet = Rect::new(front.x + offset, front.y - offset, front.w, front.h);
            // A hairline in the background colour divides the sheet from the
            // one behind it.
            ui.paint_round_rect(sheet.inset(-1.0), radius, fade(self.theme.bg, alpha));
            let item = layers.get(layer);
            self.paint_sheet(ui, sheet, radius, layer, item, alpha);
            for wash in washes {
                ui.paint_round_rect(sheet, radius, *wash);
            }
            if let Some(item) = item {
                self.request_thumb(item);
            }
        }
    }

    /// Paints an album's cover: the picture of its first track, filling the
    /// tile. An album has one cover, so there are no sheets behind it.
    /// A locked collection's cover: a grey plate with a dimmed lock. Its pictures
    /// are not in the catalogue while it is locked, so there is nothing else to draw.
    fn draw_locked_cover(&mut self, ui: &mut Ui, rect: Rect, alpha: f32, washes: &[Color]) {
        ui.paint_round_rect(rect, 8.0, fade(placeholder_sheet(self.dark, 0), alpha));
        for wash in washes {
            ui.paint_round_rect(rect, 8.0, *wash);
        }
        let side = rect.w.min(rect.h) * 0.34;
        let (cx, cy) = rect.center();
        paint_icon(
            ui,
            Rect::new(cx - side * 0.5, cy - side * 0.5, side, side),
            AppIcon::Lock,
            fade(self.theme.faint, alpha),
        );
    }

    fn draw_cover_single(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        radius: f32,
        cover: Option<&Item>,
        alpha: f32,
        washes: &[Color],
    ) {
        self.paint_sheet(ui, rect, radius, 0, cover, alpha);
        for wash in washes {
            ui.paint_round_rect(rect, radius, *wash);
        }
        if let Some(item) = cover {
            self.request_thumb(item);
        }
    }

    /// One sheet of a cover: its picture, or a grey placeholder for `layer`
    /// until the picture is ready.
    fn paint_sheet(
        &mut self,
        ui: &mut Ui,
        sheet: Rect,
        radius: f32,
        layer: usize,
        item: Option<&Item>,
        alpha: f32,
    ) {
        let texture = item.and_then(|item| self.thumbs.get(&TexKind::Thumb.key(&item.id)));
        match texture {
            Some(texture) => {
                let uv = cover_uv(&texture, sheet);
                ui.draw_list_mut().image_uv_rect(
                    sheet,
                    &texture,
                    Color::WHITE.with_alpha((255.0 * alpha) as u8),
                    radius,
                    uv,
                );
            }
            None => ui.paint_round_rect(
                sheet,
                radius,
                fade(placeholder_sheet(self.dark, layer), alpha),
            ),
        }
    }

    /// The collection sheet: a wide panel that rises from the bottom edge. A
    /// search line narrows the collections, which sit in a grid of tiles that
    /// scroll. Each tile shows its collection's cover behind its name. The
    /// footer holds a plus that opens the name field for a new collection.
    fn draw_collection_sheet(&mut self, ui: &mut Ui, width: f32, height: f32, t: f32) {
        let Some(Modal::AddToCollection {
            ids,
            name,
            query,
            focus,
            creating,
            skip,
        }) = self.modal.clone()
        else {
            return;
        };
        let has_collections = !self.collections.is_empty();
        // Collections that already hold every selected item cannot gain
        // anything, so they are not offered at all.
        let offered = sheet_offers(&self.collections, &skip, "");
        let layout = collection_sheet_layout(width, height, self.collections.len(), offered.len());
        let shown = sheet_offers(&self.collections, &skip, &query);
        // The panel is a full height below its resting place and rises into it.
        let sheet = Rect::new(
            (width - layout.width) * 0.5,
            height - layout.height + (1.0 - t) * layout.height,
            layout.width,
            layout.height,
        );
        // Its bottom corners sit below the window edge, so only the top ones show.
        let surface = Rect::new(sheet.x, sheet.y, sheet.w, sheet.h + 24.0);
        self.sheet_rect = sheet;
        ui.paint_round_rect(surface, 16.0, self.theme.g1);

        let inner_x = sheet.x + SHEET_PAD;
        let inner_w = sheet.w - 2.0 * SHEET_PAD;
        let title = if has_collections {
            "Add to collection"
        } else {
            "New collection"
        };
        // Every collection sheet carries the collection mark beside its title,
        // so the dialog reads as the same thing as the page icon.
        paint_icon(
            ui,
            Rect::new(inner_x, sheet.y + 24.0, 20.0, 20.0),
            AppIcon::Collections,
            fade(self.theme.fg, t),
        );
        self.label(
            ui,
            Rect::new(inner_x + 28.0, sheet.y + 22.0, inner_w - 28.0, 24.0),
            title,
            17.0,
            FontWeight::SemiBold,
            fade(self.theme.fg, t),
        );
        if has_collections {
            // The search sits at the right of the header and moves as the Media
            // search does: a circle at rest that opens into a field from its
            // right edge, with the text fading in once there is room for it.
            let open = self.sheet_search.x.clamp(0.0, 1.0);
            let width = 38.0 + (220.0 - 38.0) * open;
            let search = Rect::new(
                sheet.max().0 - SHEET_PAD - width,
                sheet.y + 16.0,
                width,
                SHEET_FIELD_H,
            );
            self.sheet_search_rect = search;
            if open > 0.002 {
                let fill = (open * 1.8).min(1.0);
                ui.paint_round_rect(search, SHEET_FIELD_H * 0.5, fade(self.theme.g2, fill * t));
            }
            paint_icon(
                ui,
                Rect::new(search.x + 7.0, search.y + 7.0, 24.0, 24.0),
                AppIcon::Search,
                fade(self.theme.muted, t),
            );

            // Text shows once the field is wide enough, never squeezed. Blur
            // drops the focus at once, so a field that is still shutting keeps
            // its caret blinking, as the Media search does, and shows no
            // placeholder: an unfocused, empty field is closed.
            let closing = open > 0.0 && !self.sheet_search_open();
            let text_a = ((open - 0.45) / 0.35).clamp(0.0, 1.0) * t;
            let clear_w = if query.is_empty() { 0.0 } else { 31.0 };
            let text_x = search.x + 40.0;
            let avail = (search.w - 40.0 - 7.0 - clear_w).max(0.0);
            if text_a > 0.0 && avail > 4.0 {
                ui.paint_push_clip(Rect::new(text_x, search.y, avail, search.h));
                if !query.is_empty() {
                    self.label(
                        ui,
                        Rect::new(text_x, search.y, avail, search.h),
                        &query,
                        14.0,
                        FontWeight::Regular,
                        fade(self.theme.fg, text_a),
                    );
                }
                if (focus == SheetField::Search || closing) && self.time % 1.06 < 0.58 {
                    let (caret_w, _) =
                        ui.measure(&query, ui.fonts().resolve(FontWeight::Regular, false), 14.0);
                    ui.paint_round_rect(
                        Rect::new(
                            (text_x + caret_w.min(avail) + 1.0).min(search.max().0 - 12.0),
                            search.y + 10.0,
                            1.5,
                            search.h - 20.0,
                        ),
                        0.75,
                        fade(self.theme.fg, text_a),
                    );
                }
                ui.paint_pop_clip();
            }
            if !query.is_empty() && open > 0.7 {
                let clear_a = ((open - 0.7) / 0.3).clamp(0.0, 1.0) * t;
                let clear = Rect::new(search.max().0 - 31.0, search.y + 7.0, 24.0, 24.0);
                if self.modal_hover(clear) {
                    self.cursor_hand = true;
                    ui.paint_round_rect(clear, 12.0, fade(self.theme.g3, clear_a));
                }
                paint_icon(
                    ui,
                    clear.inset(5.0),
                    AppIcon::Close,
                    fade(self.theme.muted, clear_a),
                );
                if self.modal_take_press(clear) {
                    self.clear_sheet_query();
                }
            }
            if self.modal_hover(search) {
                self.cursor_hand = true;
            }
            if self.modal_take_press(search) {
                self.focus_sheet(SheetField::Search);
            }
        }

        let grid = Rect::new(sheet.x, sheet.y + layout.head, sheet.w, layout.grid);
        if grid.contains(self.input.mouse) {
            let delta = self.input.wheel + self.input.precise;
            if delta.abs() > 0.01 {
                self.sheet_scroll -= delta;
            }
        }
        self.sheet_scroll = self.sheet_scroll.clamp(0.0, layout.max_scroll);
        let columns = layout.columns;
        let tile_w = (inner_w - (columns - 1) as f32 * SHEET_TILE_GAP) / columns as f32;
        let mut chosen = None;
        ui.paint_push_clip(grid);
        if shown.is_empty() && has_collections {
            let note = if offered.is_empty() {
                "The selected items are already in all available collections".to_string()
            } else {
                format!("No collection matches \"{}\"", query.trim())
            };
            self.label_center(
                ui,
                Rect::new(inner_x, grid.y, inner_w, SHEET_TILE_H),
                &note,
                13.0,
                FontWeight::Regular,
                fade(self.theme.muted, t),
            );
        }
        for (index, collection) in shown.iter().enumerate() {
            let top = grid.y + (index / columns) as f32 * (SHEET_TILE_H + SHEET_TILE_GAP)
                - self.sheet_scroll;
            // A tile scrolled out of the grid is neither drawn nor pressed.
            if top + SHEET_TILE_H < grid.y || top > grid.max().1 {
                continue;
            }
            let tile = Rect::new(
                inner_x + (index % columns) as f32 * (tile_w + SHEET_TILE_GAP),
                top,
                tile_w,
                SHEET_TILE_H,
            );
            let hovered = grid.contains(self.input.mouse) && self.modal_hover(tile);
            let layers = self
                .covers
                .get(&collection.id.to_string())
                .cloned()
                .unwrap_or_default();
            let mut washes = Vec::new();
            if !layers.is_empty() {
                // The panel's own colour sits over the pictures, so the cover
                // reads as a background and the name stays the subject.
                washes.push(fade(self.theme.g1.with_alpha(SHEET_TILE_TINT), t));
            }
            if hovered {
                self.cursor_hand = true;
                washes.push(fade(hover_overlay(self.dark, 1.0), t));
            }
            self.draw_cover_stack(ui, tile, 12.0, &layers, t, &washes);
            self.label_ellipsis(
                ui,
                Rect::new(tile.x + 12.0, tile.max().1 - 44.0, tile.w - 24.0, 20.0),
                &collection.name,
                13.5,
                FontWeight::Medium,
                fade(self.theme.fg, t),
            );
            self.label(
                ui,
                Rect::new(tile.x + 12.0, tile.max().1 - 22.0, tile.w - 24.0, 16.0),
                &format!(
                    "{} {}",
                    collection.count,
                    if collection.count == 1 {
                        "item"
                    } else {
                        "items"
                    }
                ),
                11.5,
                FontWeight::Regular,
                fade(self.theme.muted, t),
            );
            if grid.contains(self.input.mouse) && self.modal_take_press(tile) {
                chosen = Some(collection.id);
                break;
            }
        }
        ui.paint_pop_clip();
        if let Some(collection_id) = chosen {
            self.modal = None;
            self.add_to_collection(collection_id, &ids);
            return;
        }

        // The footer row, pinned at the bottom: the name field while creating,
        // otherwise a prompt that opens it, then the plus, then Cancel.
        let row_y = sheet.max().1 - 22.0 - SHEET_FIELD_H;
        let cancel = Rect::new(sheet.max().0 - SHEET_PAD - 88.0, row_y, 88.0, SHEET_FIELD_H);
        self.sheet_cancel_rect = cancel;
        // The plus stays at the left edge of the menu. The name field grows
        // out to its right, up to Cancel, on the spring and shrinks back on close.
        let create = self.sheet_create.x.clamp(0.0, 1.0);
        let plus = Rect::new(inner_x, row_y, SHEET_FIELD_H, SHEET_FIELD_H);
        self.sheet_plus_rect = plus;
        let field_x = plus.max().0 + SHEET_GAP / 2.0;
        let field_room = (cancel.x - SHEET_GAP / 2.0 - field_x).max(0.0);
        let field_w = field_room * create;
        let field = Rect::new(field_x, row_y, field_w, SHEET_FIELD_H);
        // The field stays while its spring shrinks it back. It is drawn exactly
        // as it opens, so closing reads as the opening run backwards: the text
        // and the ring stay, and only a press while creating reaches it.
        let leaving = !creating && create > 0.0;
        if (creating || leaving) && field_w > 0.5 {
            if creating {
                self.sheet_name_rect = field;
            } else {
                self.sheet_name_rect = Rect::default();
            }
            let clicked = self.text_field(
                ui,
                field,
                &name,
                focus == SheetField::Name || leaving,
                "Collection name",
            );
            if clicked && creating {
                self.focus_sheet(SheetField::Name);
            }
        } else {
            self.sheet_name_rect = Rect::default();
        }
        // While creating, the plus is only live once there is a name to use.
        let plus_enabled = !creating || !name.trim().is_empty();
        if self.sheet_icon_button(ui, plus, AppIcon::Plus, plus_enabled, t) {
            if creating {
                self.create_from_name();
            } else {
                self.start_creating();
            }
        }
        if self.modal_button(ui, cancel, "Cancel", ModalTone::Plain, true) {
            self.modal = None;
        }
        if self.modal.is_some() && self.input.pressed && !sheet.contains(self.input.mouse) {
            self.input.pressed = false;
            self.modal = None;
        }
    }

    /// The import sheet. It rises like the collection sheet. While it copies it
    /// shows the count over a bar, with the file being copied beneath. A clean
    /// import reads "Import complete" and eases out by itself. Only a problem
    /// asks for a decision. The scrim does not dismiss the sheet, so a stray
    /// press cannot throw an import away.
    fn draw_import_sheet(&mut self, ui: &mut Ui, width: f32, height: f32, t: f32) {
        let Some(Modal::Import(session)) = self.modal.clone() else {
            return;
        };
        let sheet_w = (width - 2.0 * SHEET_MARGIN).clamp(0.0, IMPORT_SHEET_W);
        let sheet_h = import_sheet_height(&session);
        let sheet = Rect::new(
            (width - sheet_w) * 0.5,
            height - sheet_h + (1.0 - t) * sheet_h,
            sheet_w,
            sheet_h,
        );
        // Its bottom corners sit below the window edge, so only the top ones show.
        ui.paint_round_rect(
            Rect::new(sheet.x, sheet.y, sheet.w, sheet.h + 24.0),
            16.0,
            self.theme.g1,
        );

        let inner_x = sheet.x + SHEET_PAD;
        let inner_w = sheet.w - 2.0 * SHEET_PAD;
        let (title, icon) = match session.phase {
            ImportPhase::Complete => ("Import complete", AppIcon::Check),
            ImportPhase::Review | ImportPhase::Removing => ("Review import", AppIcon::Upload),
            ImportPhase::Running | ImportPhase::Stopping => ("Importing media", AppIcon::Upload),
        };
        paint_icon(
            ui,
            Rect::new(inner_x, sheet.y + 24.0, 20.0, 20.0),
            icon,
            fade(self.theme.fg, t),
        );
        self.label(
            ui,
            Rect::new(inner_x + 28.0, sheet.y + 22.0, inner_w - 28.0, 24.0),
            title,
            17.0,
            FontWeight::SemiBold,
            fade(self.theme.fg, t),
        );

        let top = sheet.y + IMPORT_BODY_TOP;
        let fg = fade(self.theme.fg, t);
        let muted = fade(self.theme.muted, t);
        match session.phase {
            ImportPhase::Running | ImportPhase::Stopping => {
                let done = session.done.min(session.total);
                let fraction = if session.total > 0 {
                    done as f32 / session.total as f32
                } else {
                    0.0
                };
                if session.total > 0 {
                    self.label_right(
                        ui,
                        Rect::new(inner_x, sheet.y + 22.0, inner_w, 24.0),
                        &format!("{done} of {}", session.total),
                        13.0,
                        FontWeight::Medium,
                        muted,
                    );
                }
                self.label_ellipsis(
                    ui,
                    Rect::new(inner_x, top, inner_w, 18.0),
                    &session.current,
                    11.5,
                    FontWeight::Regular,
                    muted,
                );
                let track = Rect::new(inner_x, top + 28.0, inner_w, 4.0);
                ui.paint_round_rect(track, 2.0, fade(self.theme.g3, t));
                if session.total > 0 {
                    // Nothing is filled until a file is done, so the bar has no
                    // stray dot at zero.
                    if done > 0 {
                        ui.paint_round_rect(
                            Rect::new(track.x, track.y, (track.w * fraction).max(4.0), 4.0),
                            2.0,
                            fg,
                        );
                    }
                } else {
                    // Until the total is known the bar sweeps, as the busy panel does.
                    let cycle = (self.time * 0.55) % 1.0;
                    let w = track.w * 0.28;
                    let x = track.x + (track.w - w) * ease_in_out(cycle);
                    ui.paint_round_rect(Rect::new(x, track.y, w, 4.0), 2.0, fg);
                }
            }
            ImportPhase::Complete => {
                let report = session.report.clone().unwrap_or_default();
                self.label(
                    ui,
                    Rect::new(inner_x, top, inner_w, 20.0),
                    &import_summary(&report),
                    13.5,
                    FontWeight::Medium,
                    fg,
                );
                let detail = import_detail(&report);
                if !detail.is_empty() {
                    self.label(
                        ui,
                        Rect::new(inner_x, top + 24.0, inner_w, 18.0),
                        &detail,
                        11.5,
                        FontWeight::Regular,
                        muted,
                    );
                }
            }
            ImportPhase::Review | ImportPhase::Removing => {
                let report = session.report.clone().unwrap_or_default();
                self.label(
                    ui,
                    Rect::new(inner_x, top, inner_w, 20.0),
                    &import_summary(&report),
                    13.5,
                    FontWeight::Medium,
                    fg,
                );
                let detail = import_detail(&report);
                if !detail.is_empty() {
                    self.label(
                        ui,
                        Rect::new(inner_x, top + 24.0, inner_w, 18.0),
                        &detail,
                        11.5,
                        FontWeight::Regular,
                        muted,
                    );
                }
                let mut y = top + 42.0;
                if !report.failures.is_empty() {
                    let count = report.failures.len();
                    self.label(
                        ui,
                        Rect::new(inner_x, top + 50.0, inner_w, 18.0),
                        &format!(
                            "{count} {} with a problem",
                            if count == 1 { "file" } else { "files" }
                        ),
                        12.0,
                        FontWeight::Medium,
                        muted,
                    );
                    for (path, error) in report.failures.iter().take(IMPORT_PROBLEMS_SHOWN) {
                        self.label_ellipsis(
                            ui,
                            Rect::new(inner_x, y + 26.0, inner_w, IMPORT_ROW_H),
                            &format!("{} — {error}", file_label(Path::new(path))),
                            11.5,
                            FontWeight::Regular,
                            muted,
                        );
                        y += IMPORT_ROW_H;
                    }
                    if count > IMPORT_PROBLEMS_SHOWN {
                        self.label(
                            ui,
                            Rect::new(inner_x, y + 26.0, inner_w, IMPORT_ROW_H),
                            &format!("and {} more", count - IMPORT_PROBLEMS_SHOWN),
                            11.5,
                            FontWeight::Regular,
                            muted,
                        );
                        y += IMPORT_ROW_H;
                    }
                    y += 26.0;
                }
                if let Some(error) = session.error.as_ref() {
                    self.label_ellipsis(
                        ui,
                        Rect::new(inner_x, y + 8.0, inner_w, IMPORT_ROW_H),
                        error,
                        11.5,
                        FontWeight::Medium,
                        fade(self.theme.danger, t),
                    );
                }
            }
        }

        // The button row is pinned to the bottom, as the collection sheet's is.
        // A running copy offers only Cancel. A problem offers Cancel beside
        // Confirm, and a finished clean import has no buttons at all.
        let row_y = sheet.max().1 - 22.0 - SHEET_FIELD_H;
        let primary = Rect::new(
            sheet.max().0 - SHEET_PAD - 120.0,
            row_y,
            120.0,
            SHEET_FIELD_H,
        );
        let secondary = Rect::new(
            primary.x - SHEET_GAP / 2.0 - 120.0,
            row_y,
            120.0,
            SHEET_FIELD_H,
        );
        match session.phase {
            ImportPhase::Running => {
                if self.modal_button(ui, primary, "Cancel", ModalTone::Plain, true) {
                    self.cancel_import();
                }
            }
            ImportPhase::Stopping => {
                self.modal_button(ui, primary, "Stopping…", ModalTone::Plain, false);
            }
            ImportPhase::Review => {
                if self.modal_button(ui, secondary, "Cancel", ModalTone::Plain, true) {
                    self.cancel_import();
                }
                if self.modal_button(ui, primary, "Confirm", ModalTone::Primary, true) {
                    self.confirm_import();
                }
            }
            ImportPhase::Removing => {
                self.modal_button(ui, secondary, "Removing…", ModalTone::Plain, false);
                self.modal_button(ui, primary, "Confirm", ModalTone::Primary, false);
            }
            ImportPhase::Complete => {}
        }
    }

    /// Shows the new-collection name field and puts the cursor in it.
    fn start_creating(&mut self) {
        if let Some(Modal::AddToCollection {
            creating, focus, ..
        }) = self.modal.as_mut()
        {
            *creating = true;
            *focus = SheetField::Name;
        }
        self.dirty = true;
    }

    /// A square icon button for the sheet's footer. Filled when it can act;
    /// returns true when it is pressed.
    fn sheet_icon_button(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        icon: AppIcon,
        enabled: bool,
        t: f32,
    ) -> bool {
        let hovered = enabled && self.modal_hover(rect);
        let fill = if !enabled {
            self.theme.g2
        } else if hovered {
            self.theme.fg.lighten(0.08)
        } else {
            self.theme.fg
        };
        ui.paint_round_rect(rect, 10.0, fade(fill, t));
        paint_icon(
            ui,
            center_in(rect, (18.0, 18.0)),
            icon,
            fade(
                if enabled {
                    self.theme.accent_text
                } else {
                    self.theme.faint
                },
                t,
            ),
        );
        if hovered {
            self.cursor_hand = true;
        }
        enabled && self.modal_take_press(rect)
    }

    fn text_field(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        focused: bool,
        placeholder: &str,
    ) -> bool {
        ui.paint_round_rect(rect, 8.0, self.theme.g2);
        if focused {
            ui.paint_round_ring(rect, 8.0, 1.5, self.theme.fg);
        }
        let ink = if text.is_empty() {
            self.theme.faint
        } else {
            self.theme.fg
        };
        let shown = if text.is_empty() { placeholder } else { text };
        // The text is clipped to the box, so while the box is narrower than the
        // text it is cut off at the edge rather than spilling past it.
        ui.paint_push_clip(rect);
        self.label(
            ui,
            Rect::new(rect.x + 12.0, rect.y, rect.w - 24.0, rect.h),
            shown,
            13.0,
            FontWeight::Regular,
            ink,
        );
        if focused && self.time % 1.06 < 0.58 {
            let (text_width, _) =
                ui.measure(text, ui.fonts().resolve(FontWeight::Regular, false), 13.0);
            ui.paint_round_rect(
                Rect::new(
                    (rect.x + 12.0 + text_width + 1.0).min(rect.max().0 - 12.0),
                    rect.y + 10.0,
                    1.5,
                    rect.h - 20.0,
                ),
                0.75,
                self.theme.fg,
            );
        }
        ui.paint_pop_clip();
        let clicked = self.modal_hover(rect) && self.input.pressed;
        if clicked {
            self.input.pressed = false;
            self.dirty = true;
        }
        clicked
    }

    // --- Toast ----------------------------------------------------------

    fn draw_toast(&mut self, ui: &mut Ui, width: f32, height: f32) {
        let Some(toast) = &self.toast else {
            return;
        };
        let age = toast.age;
        let alpha = if age < 0.18 {
            age / 0.18
        } else if age > 3.8 {
            ((4.2 - age) / 0.4).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let (text_width, _) = ui.measure(
            &toast.text,
            ui.fonts().resolve(FontWeight::Medium, false),
            12.5,
        );
        // Centre on the content area, the same axis as the selection bar, so
        // bottom-centre surfaces line up with each other.
        let content_w = (width - SIDEBAR_W).max(0.0);
        let w = (text_width + 32.0).min(content_w - 64.0);
        let selecting = self.select_mode && !self.selection.is_empty() && !self.overlay();
        // Clear the selection bar (48px tall, 28px from the bottom) with a 10px gap.
        let lift = if selecting { 62.0 } else { 0.0 };
        let bottom = height - 24.0 - lift;
        let card = Rect::new(
            SIDEBAR_W + (content_w - w) * 0.5,
            bottom - 40.0 + (1.0 - alpha) * 6.0,
            w,
            40.0,
        );
        ui.paint_round_rect(card, 10.0, fade(self.theme.fg, alpha));
        let style = self
            .text_style(
                ui,
                12.5,
                FontWeight::Medium,
                fade(self.theme.accent_text, alpha),
            )
            .align(Align::Center);
        ui.label(card.inset_xy(14.0, 0.0), &toast.text, style);
    }

    // --- Selfcheck -------------------------------------------------------

    pub fn selfcheck() -> Result<String, String> {
        let root = std::env::temp_dir().join(format!(
            "forbiddenlibrary-selfcheck-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let source = root.join("source");
        std::fs::create_dir_all(&source).map_err(|e| e.to_string())?;
        let mut paths = Vec::new();
        for index in 0..6u32 {
            let mut pixels = vec![0u8; 32 * 32 * 4];
            for (position, px) in pixels.chunks_exact_mut(4).enumerate() {
                let x = position % 32;
                let y = position / 32;
                px[0] = (x * 8) as u8;
                px[1] = (y * 8) as u8;
                px[2] = (index * 40) as u8;
                px[3] = 255;
            }
            let path = source.join(format!("photo-{index}.png"));
            std::fs::write(&path, dream::png::encode(32, 32, &pixels))
                .map_err(|e| e.to_string())?;
            paths.push(path);
        }
        let library_root = root.join("library");
        let settings = Settings {
            library_root: Some(library_root.clone()),
            ..Default::default()
        };
        let mut app = Self::headless(library_root, settings)?;
        let report = archive_core::import_paths(
            &app.library,
            &paths,
            &archive_core::ImportOptions::default(),
            &mut |_| {},
        );
        if report.imported != 6 {
            return Err(format!(
                "selfcheck import: expected 6, got {} ({} failed)",
                report.imported, report.failed
            ));
        }
        app.reload();
        if app.display.len() != 6 {
            return Err(format!(
                "selfcheck catalogue: expected 6 items, got {}",
                app.display.len()
            ));
        }
        let mut pages = 0;
        for page in [Page::Media, Page::Collections, Page::Trash] {
            app.set_page(page.clone());
            app.screen = (1440.0, 900.0);
            let frame = app.build_frame(0.016);
            if frame.cmds.is_empty() {
                return Err(format!("selfcheck page {:?} drew nothing", page));
            }
            pages += 1;
        }

        // An empty page still draws, and offers no buttons of its own. The
        // trash keeps its retention line in the page header, not in the state.
        for page in [Page::Media, Page::Trash] {
            app.set_page(page.clone());
            app.display.clear();
            let frame = app.build_frame(0.016);
            if frame.cmds.is_empty() {
                return Err(format!("selfcheck empty {:?} page: drew nothing", page));
            }
        }
        app.set_page(Page::Media);

        // The sidebar must be live: click the Media row from Collections.
        app.set_page(Page::Collections);
        app.pointer_moved((SIDEBAR_W * 0.5, 40.0));
        app.pointer_button(true);
        app.build_frame(0.016);
        app.pointer_button(false);
        app.build_frame(0.016);
        if app.page != Page::Media {
            return Err(format!(
                "selfcheck navigation: expected Media, got {:?}",
                app.page
            ));
        }

        // Search, selection and the viewer. Date queries stay structured;
        // free text goes through the semantic index and is not exercised
        // here (it needs the bundled model).
        let Some(first) = app.items.first() else {
            return Err("selfcheck search: no items to search".into());
        };
        let date = archive_core::format_date(first.captured_at);
        app.search = date.clone();
        app.filter_display();
        if app.display.is_empty() {
            return Err(format!("selfcheck search: no match for {date:?}"));
        }
        if app.display.iter().any(|item| {
            !archive_core::format_date(item.captured_at)
                .to_lowercase()
                .contains(&date.to_lowercase())
                && !archive_core::date_key(item.captured_at).contains(&date)
        }) {
            return Err("selfcheck search: date query matched an unrelated item".into());
        }

        // The field opens out of its icon when it holds text, stays open while
        // it is selected with nothing typed, and closes once it is empty and
        // deselected with the pointer away.
        app.search_focus = true;
        app.search_hover = false;
        for _ in 0..40 {
            app.animate(0.016);
            app.build_frame(0.016);
        }
        if !app.search_open.settled(1.0) {
            return Err(format!(
                "selfcheck search: field stayed shut ({})",
                app.search_open.x
            ));
        }
        app.clear_search();
        for _ in 0..40 {
            app.animate(0.016);
            app.build_frame(0.016);
        }
        if !app.search_open.settled(1.0) {
            return Err(format!(
                "selfcheck search: closed while selected ({})",
                app.search_open.x
            ));
        }
        app.search_focus = false;
        for _ in 0..40 {
            app.animate(0.016);
            app.build_frame(0.016);
        }
        if !app.search_open.settled(0.0) {
            return Err(format!(
                "selfcheck search: field stayed open ({})",
                app.search_open.x
            ));
        }

        // A real click into the field, then the pointer walking away with
        // nothing typed: selection alone has to keep it open.
        app.build_frame(0.016);
        let icon = app.search_rect.center();
        app.pointer_moved(icon);
        for _ in 0..20 {
            app.animate(0.016);
            app.build_frame(0.016);
        }
        app.pointer_button(true);
        app.build_frame(0.016);
        app.pointer_button(false);
        app.build_frame(0.016);
        if !app.search_focus {
            return Err("selfcheck search: clicking the field did not select it".into());
        }
        app.pointer_moved((app.screen.0 - 40.0, app.screen.1 - 40.0));
        for _ in 0..30 {
            app.animate(0.016);
            app.build_frame(0.016);
        }
        if !app.search_open.settled(1.0) || !app.search_focus {
            return Err(format!(
                "selfcheck search: closed after the pointer left ({}, focus {})",
                app.search_open.x, app.search_focus
            ));
        }
        app.search_focus = false;
        for _ in 0..30 {
            app.animate(0.016);
            app.build_frame(0.016);
        }
        app.open_viewer(0);
        if let Some(viewer) = app.viewer.as_mut() {
            viewer.info_open = true;
        }
        for _ in 0..10 {
            app.build_frame(0.016);
        }
        if app.viewer.is_none() {
            return Err("selfcheck viewer: did not open".into());
        }
        app.close_viewer();
        app.select_mode = true;
        app.selection.insert(app.display[0].id.clone());
        let frame = app.build_frame(0.016);
        if frame.cmds.is_empty() {
            return Err("selfcheck selection: selection bar drew nothing".into());
        }
        app.end_selection();

        // Modals render with their own input pass.
        app.modal = Some(Modal::AddToCollection {
            ids: app.display[0..1]
                .iter()
                .map(|item| item.id.clone())
                .collect(),
            name: "Trip".into(),
            query: String::new(),
            focus: SheetField::Name,
            creating: true,
            skip: Default::default(),
        });
        app.modal_t = 1.0;
        let frame = app.build_frame(0.016);
        if frame.cmds.is_empty() {
            return Err("selfcheck modal: drew nothing".into());
        }
        app.modal = None;

        // Trash explains its retention and draws the per-item countdown.
        let trashed = app.display[0].id.clone();
        app.library.trash(&[trashed]).map_err(|e| e.to_string())?;
        app.set_page(Page::Trash);
        if app.display.len() != 1 {
            return Err(format!(
                "selfcheck trash: expected 1 trashed item, got {}",
                app.display.len()
            ));
        }
        let frame = app.build_frame(0.016);
        if frame.cmds.is_empty() {
            return Err("selfcheck trash: drew nothing".into());
        }

        let result = format!(
            "selfcheck ok · {pages} pages · {report:?} imported · {} vertices in the last frame",
            frame.vertices.len()
        );
        app.ui = None;
        drop(app.library);
        let _ = std::fs::remove_dir_all(&root);
        Ok(result)
    }
}

/// The centred source rectangle that fills `rect` from edge to edge while
/// keeping the image's aspect ratio: the smallest crop that avoids any
/// stretching.
/// The grey of a placeholder sheet. Each layer is a little darker or lighter
/// than the one in front, so a stack of placeholders still reads as sheets.
fn placeholder_sheet(dark: bool, layer: usize) -> Color {
    let shade = 7 * layer as u8;
    let v = if dark { 0x3c - shade } else { 0xd4 - shade };
    Color::rgb(v, v, v)
}

fn cover_uv(texture: &dream::texture::GpuTexture, rect: Rect) -> (f32, f32, f32, f32) {
    cover_uv_for(texture.size(), (rect.w, rect.h))
}

fn cover_uv_for(size: (u32, u32), rect: (f32, f32)) -> (f32, f32, f32, f32) {
    let (width, height) = size;
    let source = width.max(1) as f32 / height.max(1) as f32;
    let target = (rect.0.max(1.0) / rect.1.max(1.0)).max(0.0001);
    if source > target {
        let span = target / source;
        let start = (1.0 - span) * 0.5;
        (start, 0.0, start + span, 1.0)
    } else {
        let span = source / target;
        let start = (1.0 - span) * 0.5;
        (0.0, start, 1.0, start + span)
    }
}

/// What the viewer draws from its media in one frame, copied out so the
/// drawing can borrow the app again.
#[derive(Default)]
struct MediaState {
    kind: MediaKind,
    /// The open file has sound and no picture stream: music.
    audio_only: bool,
    frame: Option<Arc<GpuTexture>>,
    full: Option<Arc<GpuTexture>>,
    status: Option<PlayerStatus>,
    /// Display size the decoder reports, once the file has been probed.
    display: Option<(u32, u32)>,
    position_ms: i64,
    duration_ms: Option<i64>,
    playing: bool,
    muted: bool,
    volume: f32,
}

/// The height of the subtitle band above the video bar, and the gap between it
/// and the cover or the bar.
const LYRIC_BAND: f32 = 56.0;
const LYRIC_GAP: f32 = 10.0;
const LYRIC_SIZE: f32 = 20.0;
/// The black outline: eight directions, `LYRIC_OUTLINE_PX` out from the text.
const LYRIC_OUTLINE: [(f32, f32); 8] = [
    (-1.0, 0.0),
    (1.0, 0.0),
    (0.0, -1.0),
    (0.0, 1.0),
    (-1.0, -1.0),
    (1.0, -1.0),
    (-1.0, 1.0),
    (1.0, 1.0),
];
const LYRIC_OUTLINE_PX: f32 = 1.5;

fn is_animated_ext(ext: &str) -> bool {
    ANIM_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Upload one decoded animation or video frame. Frames are sized to the
/// display already, so they take a single mip level.
fn upload_frame(
    context: &Arc<GpuContext>,
    label: &str,
    frame: &DecodedFrame,
) -> Option<Arc<GpuTexture>> {
    thumbs::upload(
        context,
        label,
        frame.width,
        frame.height,
        &frame.rgba,
        Some(1),
    )
    .map(Arc::new)
}

/// The largest rectangle of `aspect` that fits inside `area`, centred.
pub(crate) fn fit_in(area: Rect, aspect: f32) -> Rect {
    let aspect = aspect.max(0.0001);
    let mut width = area.h * aspect;
    let mut height = area.h;
    if width > area.w {
        width = area.w;
        height = width / aspect;
    }
    center_in(area, (width, height))
}

/// `rect` moved right by `dx`, for glyphs whose visual weight sits left of
/// their box centre, such as a play triangle.
fn optical_nudge(rect: Rect, dx: f32) -> Rect {
    Rect::new(rect.x + dx, rect.y, rect.w, rect.h)
}

fn center_in(rect: Rect, size: (f32, f32)) -> Rect {
    Rect::new(
        rect.x + (rect.w - size.0) * 0.5,
        rect.y + (rect.h - size.1) * 0.5,
        size.0,
        size.1,
    )
}

/// The card's size for `rows`: as wide as its longest label, never narrower
/// than the minimum, and as tall as its rows stack.
fn context_menu_size(ui: &mut Ui, rows: &[MenuRow]) -> (f32, f32) {
    let mut width = MENU_MIN_W;
    let mut height = MENU_PAD * 2.0;
    for row in rows {
        match row {
            MenuRow::Header(text) => {
                width = width.max(measure_text_width(ui, text, 12.0) + MENU_ICON_X * 2.0);
                height += MENU_HEADER_H;
            }
            MenuRow::Item { label, .. } => {
                width = width.max(measure_text_width(ui, label, 13.5) + MENU_LABEL_X + 16.0);
                height += MENU_ROW_H;
            }
        }
    }
    (width.min(MENU_MAX_W), height)
}

/// Where a card of `size` sits for a press at `at`. It opens down and to the
/// right of the pointer, and turns back toward it where the window has no room.
fn menu_place(at: (f32, f32), size: (f32, f32), screen: (f32, f32)) -> Rect {
    let (w, h) = size;
    let x = if at.0 + w + MENU_MARGIN <= screen.0 {
        at.0
    } else {
        at.0 - w
    };
    let y = if at.1 + h + MENU_MARGIN <= screen.1 {
        at.1
    } else {
        at.1 - h
    };
    let x = x.clamp(MENU_MARGIN, (screen.0 - w - MENU_MARGIN).max(MENU_MARGIN));
    let y = y.clamp(MENU_MARGIN, (screen.1 - h - MENU_MARGIN).max(MENU_MARGIN));
    Rect::new(x, y, w, h)
}

/// The rect of each row inside `card`, top to bottom, in the order the rows
/// are drawn.
fn menu_row_rects(card: Rect, rows: &[MenuRow]) -> Vec<Rect> {
    let mut y = card.y + MENU_PAD;
    rows.iter()
        .map(|row| {
            let h = match row {
                MenuRow::Header(_) => MENU_HEADER_H,
                MenuRow::Item { .. } => MENU_ROW_H,
            };
            let rect = Rect::new(card.x + MENU_PAD, y, card.w - MENU_PAD * 2.0, h);
            y += h;
            rect
        })
        .collect()
}

fn measure_text_width(ui: &mut Ui, text: &str, size: f32) -> f32 {
    let font = ui.fonts().resolve(FontWeight::Medium, false);
    ui.measure(text, font, size).0
}

/// How much of its hit area a bar icon's 24 grid fills.
const BAR_GLYPH: f32 = 0.75;

/// The black outline around the video bar's controls, in grid units and in
/// pixels at this size (the grid is 24 units across a 24 px glyph). It grows
/// each glyph and track by this much on every side. Built out with the
/// `plain-controls` feature, which draws the controls without it.
#[cfg(not(feature = "plain-controls"))]
const BAR_OUTLINE: f32 = 1.0;

/// The outline's opacity as a share of the control's own ink.
#[cfg(not(feature = "plain-controls"))]
const BAR_OUTLINE_ALPHA: f32 = 0.5;

/// One layer of a bar icon on its 24 grid: the glyph at `amount` (0 hidden to
/// 1 full) grows from `anchor`, a point of the grid as fractions, while it
/// fades in. Every layer shares the grid, so layers line up exactly.
fn bar_glyph(ui: &mut Ui, rect: Rect, icon: AppIcon, amount: f32, anchor: (f32, f32), ink: Color) {
    let amount = amount.clamp(0.0, 1.0);
    if amount < 0.01 {
        return;
    }
    // Every layer is first fitted to the same grid square, then grown alone.
    let side = rect.w * BAR_GLYPH;
    let grid = Rect::new(
        rect.x + (rect.w - side) * 0.5,
        rect.y + (rect.h - side) * 0.5,
        side,
        side,
    );
    let scale = 0.6 + 0.4 * amount;
    let ax = grid.x + grid.w * anchor.0;
    let ay = grid.y + grid.h * anchor.1;
    let scaled = Rect::new(
        ax + (grid.x - ax) * scale,
        ay + (grid.y - ay) * scale,
        grid.w * scale,
        grid.h * scale,
    );
    let ink = Color {
        a: (ink.a as f32 * amount).round() as u8,
        ..ink
    };
    let fill = matches!(icon, AppIcon::Play | AppIcon::Pause | AppIcon::VolumeBody);
    #[cfg(not(feature = "plain-controls"))]
    paint_grid_outlined(
        ui,
        scaled,
        side,
        icon,
        fill,
        Color::rgba(0, 0, 0, (ink.a as f32 * BAR_OUTLINE_ALPHA).round() as u8),
        BAR_OUTLINE,
    );
    paint_grid(ui, scaled, side, icon, fill, ink);
}

fn duration_label(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    let seconds = total % 60;
    let minutes = (total / 60) % 60;
    let hours = total / 3600;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A query that is entirely a date: digits plus month names. Everything else
/// is handed to the semantic index.
fn is_date_query(query: &str) -> bool {
    const MONTHS: [&str; 24] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
        "jan",
        "feb",
        "mar",
        "apr",
        "jun",
        "jul",
        "aug",
        "sep",
        "sept",
        "oct",
        "nov",
        "dec",
    ];
    let mut saw_component = false;
    for word in query.split(|character: char| !character.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if word.chars().all(|character| character.is_ascii_digit()) {
            saw_component = true;
            continue;
        }
        if MONTHS.contains(&word.to_ascii_lowercase().as_str()) {
            saw_component = true;
            continue;
        }
        return false;
    }
    saw_component
}

#[cfg(test)]
mod tests {
    use super::{
        ArchiveApp, EmptyKind, IMPORT_COMPLETE_HOLD, ImportPhase, ImportSession, MenuAction,
        MenuRow, Modal, Page, PasswordMode, SHEET_TILE_H, SheetField, TIMELINE_PAGE, TITLE_STEP,
        VAULT_IDLE, collection_sheet_layout, cover_uv_for, fit_in, is_date_query, menu_place,
        name_roll, sheet_offers,
    };
    use crate::settings::Settings;
    use archive_core::{Collection, LockState, TimelineFilter};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn close(actual: (f32, f32, f32, f32), expected: (f32, f32, f32, f32)) {
        for (left, right) in [
            (actual.0, expected.0),
            (actual.1, expected.1),
            (actual.2, expected.2),
            (actual.3, expected.3),
        ] {
            assert!((left - right).abs() < 1e-5, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn the_page_name_rides_the_drum_when_the_island_turns_away() {
        // Album (row 1) closing as the island moves to Collections (row 2).
        // The name must track its drum slot exactly, so it rolls out with the rows.
        for step in 0..=20 {
            let p = step as f32 / 20.0;
            let roll = name_roll(1.0, 1.0 + p, 1.0 - p);
            let drum = -p * TITLE_STEP;
            assert!(
                (roll - drum).abs() < 0.1 * TITLE_STEP,
                "p={p}: name at {roll}, drum slot at {drum}"
            );
        }
        // Collection (row 2) closing as the island moves back to Albums (row 1).
        for step in 0..=20 {
            let p = step as f32 / 20.0;
            let roll = name_roll(2.0, 2.0 - p, 1.0 - p);
            let drum = p * TITLE_STEP;
            assert!(
                (roll - drum).abs() < 0.1 * TITLE_STEP,
                "p={p}: name at {roll}, drum slot at {drum}"
            );
        }
    }

    #[test]
    fn the_page_name_rolls_in_and_out_on_its_own_row() {
        // Opening from rest rolls the name up from below its slot.
        assert!((name_roll(2.0, 2.0, 0.0) - TITLE_STEP).abs() < 1e-5);
        assert!(name_roll(2.0, 2.0, 1.0).abs() < 1e-5);
        // Going back to the list on the same row rolls the name down and out.
        assert!((name_roll(1.0, 1.0, 0.0) - TITLE_STEP).abs() < 1e-5);
    }

    #[test]
    fn import_source_picker_and_qr_scrim_cancel_without_leaving_a_listener() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = ArchiveApp::headless(temp.path().to_path_buf(), Settings::default()).unwrap();
        app.screen = (1040.0, 660.0);
        app.choose_import_source();
        assert!(matches!(app.modal, Some(Modal::ImportSource)));
        app.modal_t = 1.0;
        app.build_frame(0.0);
        let session = crate::remote_import::Session::test_session();
        let address = session.address.clone();
        let staging = session.staging.path().to_path_buf();
        let weak = Arc::downgrade(&session);
        app.modal = Some(Modal::RemoteImport(session));
        app.build_frame(0.0);
        // The subtle URL reveals certificate details without ending the session.
        app.input.mouse = (520.0, 482.0);
        app.input.pressed = true;
        app.build_frame(0.0);
        assert!(matches!(app.modal, Some(Modal::RemoteImportDetails(_))));
        assert!(weak.upgrade().is_some());
        app.input.mouse = (1.0, 1.0);
        app.input.pressed = true;
        app.build_frame(0.0);
        assert!(app.modal.is_none());
        assert!(weak.upgrade().is_none());
        assert!(!staging.exists());
        assert!(std::net::TcpStream::connect(address).is_err());
    }

    #[test]
    fn a_completed_wireless_transfer_enters_the_normal_import_flow() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = ArchiveApp::headless(temp.path().to_path_buf(), Settings::default()).unwrap();
        let session = crate::remote_import::Session::test_session();
        let address = session.address.clone();
        session.test_complete();
        // Finishing should also work while connection details are being viewed.
        app.modal = Some(Modal::RemoteImportDetails(session));
        app.on_remote_import_changed();
        assert!(matches!(app.modal, Some(Modal::Import(_))));
        assert!(std::net::TcpStream::connect(address).is_err());
        app.modal = None;
        // A late status wake-up must never reopen a dismissed dialog.
        app.on_remote_import_changed();
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_new_empty_message_dissolves_instead_of_snapping() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut app = ArchiveApp::headless(temp.path().to_path_buf(), Settings::default()).unwrap();
        app.screen = (1440.0, 900.0);
        app.display.clear();

        app.build_frame(1.0 / 60.0);
        assert_eq!(app.empty_state, Some(EmptyKind::Media));
        assert!(
            app.empty_t > 0.0 && app.empty_t < 1.0,
            "the message should still be fading in, not fully drawn"
        );

        // A search that finds nothing swaps the message; the new one starts
        // its own dissolve rather than appearing at full strength.
        app.search = "nothing matches this".into();
        app.display.clear();
        app.build_frame(1.0 / 60.0);
        assert_eq!(app.empty_state, Some(EmptyKind::NoMatches));
        assert_eq!(app.empty_prev, Some(EmptyKind::Media));
        assert!(
            app.empty_t < 0.2,
            "the dissolve should restart from nothing"
        );

        for _ in 0..40 {
            app.build_frame(1.0 / 60.0);
        }
        assert_eq!(app.empty_t, 1.0);
        assert_eq!(app.empty_prev, None, "only the new message is left");
        assert_eq!(app.empty_state, Some(EmptyKind::NoMatches));
    }

    /// A library larger than one page must open on the first page only, then
    /// extend in order as the scroll asks for more. This drives the same code
    /// the window does, without a renderer.
    #[test]
    fn the_timeline_loads_one_bounded_page_and_extends_in_order() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let mut files = Vec::new();
        for index in 0..(TIMELINE_PAGE + 70) {
            let path = source.join(format!("photo-{index:04}.png"));
            let pixels: Vec<u8> = (0..8 * 8)
                .flat_map(|pixel| {
                    let value = ((index + pixel) % 256) as u8;
                    [value, (index % 251) as u8, 40, 255]
                })
                .collect();
            std::fs::write(&path, dream::png::encode(8, 8, &pixels)).unwrap();
            files.push(path);
        }
        let mut app = ArchiveApp::headless(source.join("library"), Settings::default()).unwrap();
        let report = archive_core::import_paths(
            &app.library,
            &files,
            &archive_core::ImportOptions::default(),
            &mut |_| {},
        );
        assert_eq!(report.imported, TIMELINE_PAGE + 70, "{report:?}");

        app.reload();
        assert_eq!(app.timeline_total, TIMELINE_PAGE + 70);
        assert_eq!(
            app.items.len(),
            TIMELINE_PAGE,
            "the first frame reads one page, not the whole library"
        );
        assert!(!app.timeline_done);
        assert_eq!(app.display.len(), app.items.len());

        let mut ids: Vec<String> = app.items.iter().map(|item| item.id.clone()).collect();
        for _ in 0..4 {
            app.load_more_timeline();
            if app.timeline_done {
                break;
            }
        }
        assert!(app.timeline_done, "the rest of the catalogue must load");
        assert_eq!(app.items.len(), TIMELINE_PAGE + 70);
        assert_eq!(app.display.len(), app.items.len());
        ids.extend(
            app.items[TIMELINE_PAGE..]
                .iter()
                .map(|item| item.id.clone()),
        );
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "a page boundary duplicated a row");
        let whole = app
            .library
            .items(&archive_core::TimelineFilter::default())
            .unwrap();
        assert_eq!(
            ids,
            whole.iter().map(|item| item.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unselecting_the_last_tile_leaves_selection_mode() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut app = ArchiveApp::headless(temp.path().to_path_buf(), Settings::default()).unwrap();

        // Selecting a tile is what puts the app into selection mode.
        app.toggle_selection("one", 7);
        assert!(
            app.select_mode,
            "a selection should raise the selection bar"
        );
        assert_eq!(app.selection.len(), 1);
        assert_eq!(app.last_selected, Some(7));

        // Taking it back out leaves nothing to act on, so the bar goes too,
        // and the shift-click anchor goes with it: the next selection must not
        // range from a tile picked in the session that just ended.
        app.toggle_selection("one", 7);
        assert!(app.selection.is_empty());
        assert!(!app.select_mode, "the bar should close with the last tile");
        assert_eq!(
            app.last_selected, None,
            "the old anchor should be forgotten"
        );
    }

    #[test]
    fn the_trash_bar_offers_no_add_to_collection_button() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut app = ArchiveApp::headless(temp.path().to_path_buf(), Settings::default()).unwrap();
        app.screen = (1440.0, 900.0);
        app.set_page(Page::Trash);
        app.select_mode = true;
        app.selection.insert("gone".into());

        // Where the collection button sits in the bar on the media pages, between
        // the select-all label and the restore action.
        app.input.mouse = (1356.0, 33.0);
        app.input.pressed = true;
        app.build_frame(1.0 / 60.0);

        assert!(
            app.modal.is_none(),
            "the trash bar must not offer a collection for trashed items"
        );
    }

    #[test]
    fn date_queries_are_recognised() {
        assert!(is_date_query("October"));
        assert!(is_date_query("23 October 2026"));
        assert!(is_date_query("2024-10"));
        assert!(!is_date_query("black cat"));
        assert!(!is_date_query("cat 2024"));
    }

    #[test]
    fn a_square_tile_centre_crops_without_stretching() {
        // 4:3 source in a square: a quarter of the width goes, half each side.
        close(
            cover_uv_for((1200, 900), (200.0, 200.0)),
            (0.125, 0.0, 0.875, 1.0),
        );
        // Portrait source in a square: the top and bottom are cropped.
        close(
            cover_uv_for((900, 1200), (200.0, 200.0)),
            (0.0, 0.125, 1.0, 0.875),
        );
    }

    #[test]
    fn the_viewer_fits_a_portrait_photo_inside_the_area() {
        let area = dream::ui::Rect::new(80.0, 92.0, 1000.0, 600.0);
        let portrait = fit_in(area, 0.75);
        assert!((portrait.w - 450.0).abs() < 1e-4);
        assert!((portrait.h - 600.0).abs() < 1e-4);
        assert!((portrait.x - (80.0 + 550.0 / 2.0)).abs() < 1e-4);
        assert!((portrait.y - 92.0).abs() < 1e-4);

        // A wide image is bounded by the area's width instead.
        let panorama = fit_in(area, 3.0);
        assert!((panorama.w - 1000.0).abs() < 1e-4);
        assert!((panorama.h - 1000.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn a_matching_aspect_keeps_the_whole_image() {
        close(
            cover_uv_for((1600, 900), (320.0, 180.0)),
            (0.0, 0.0, 1.0, 1.0),
        );
        close(cover_uv_for((800, 800), (64.0, 64.0)), (0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn the_first_collection_is_made_from_the_selection_sheet() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let path = source.join("photo.png");
        std::fs::write(&path, dream::png::encode(16, 16, &vec![128u8; 16 * 16 * 4])).unwrap();
        let library_root = temp.path().join("library");
        let settings = Settings {
            library_root: Some(library_root.clone()),
            ..Default::default()
        };
        let mut app = ArchiveApp::headless(library_root, settings).unwrap();
        let report = archive_core::import_paths(
            &app.library,
            &[path],
            &archive_core::ImportOptions::default(),
            &mut |_| {},
        );
        assert_eq!(report.imported, 1, "the fixture should import");
        app.reload();
        let id = app.display[0].id.clone();

        // With no collections there is nothing to pick, so the sheet opens on
        // the name field: adding media is how the first collection is made.
        app.ask_add_to_collection(vec![id.clone()]);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection {
                    focus: SheetField::Name,
                    ..
                })
            ),
            "the first collection should start on its name"
        );
        if let Some(Modal::AddToCollection { name, .. }) = app.modal.as_mut() {
            name.push_str("Trip");
        }
        app.confirm_modal();

        let collection = app
            .collections
            .iter()
            .find(|collection| collection.name == "Trip")
            .cloned()
            .expect("the sheet should create the collection");
        assert_eq!(collection.count, 1);
        // The cover is a piece of media from the collection, so a collection
        // born this way is never a blank tile.
        assert_eq!(collection.cover_item_ids, vec![id.clone()]);

        // The only collection already holds this item, so it has nothing to
        // add and the sheet offers the name field instead.
        app.ask_add_to_collection(vec![id]);
        assert!(
            matches!(&app.modal, Some(Modal::AddToCollection { focus: SheetField::Name, skip, .. }) if skip.contains(&collection.id)),
            "a collection that already holds the item should not be offered"
        );
    }

    /// Clicks the sheet's search icon the way a person would: a frame to lay
    /// the sheet out, a press on the icon, and a frame to act on it.
    fn click_sheet_search(app: &mut ArchiveApp) {
        app.screen = (1440.0, 900.0);
        app.modal_t = 1.0;
        app.build_frame(1.0 / 60.0);
        app.input.mouse = app.sheet_search_rect.center();
        app.pointer_button(true);
        app.pointer_button(false);
        app.build_frame(1.0 / 60.0);
    }

    fn app_with_photos(count: usize) -> (tempfile::TempDir, ArchiveApp, Vec<String>) {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let mut paths = Vec::new();
        for index in 0..count {
            let mut pixels = vec![128u8; 16 * 16 * 4];
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[0] = (index * 40) as u8;
                pixel[1] = (index * 70) as u8;
                pixel[2] = (index * 110) as u8;
            }
            let path = source.join(format!("photo-{index}.png"));
            std::fs::write(&path, dream::png::encode(16, 16, &pixels)).unwrap();
            paths.push(path);
        }
        let library_root = temp.path().join("library");
        let settings = Settings {
            library_root: Some(library_root.clone()),
            ..Default::default()
        };
        let mut app = ArchiveApp::headless(library_root, settings).unwrap();
        let report = archive_core::import_paths(
            &app.library,
            &paths,
            &archive_core::ImportOptions::default(),
            &mut |_| {},
        );
        assert_eq!(report.imported, count, "the fixture should import");
        app.reload();
        let ids = app.items.iter().map(|item| item.id.clone()).collect();
        (temp, app, ids)
    }

    #[test]
    fn the_sheet_hides_collections_that_already_hold_everything() {
        let (_temp, mut app, ids) = app_with_photos(2);
        let first = ids[0].clone();
        let second = ids[1].clone();
        let full = app.library.create_collection("Full").unwrap();
        let half = app.library.create_collection("Half").unwrap();
        let empty = app.library.create_collection("Empty").unwrap();
        app.library
            .add_to_collection(full, &[first.clone(), second.clone()])
            .unwrap();
        app.library
            .add_to_collection(half, std::slice::from_ref(&first))
            .unwrap();
        app.reload();

        app.ask_add_to_collection(vec![first.clone(), second]);
        let Some(Modal::AddToCollection { skip, focus, .. }) = &app.modal else {
            panic!("the collection sheet should be open");
        };
        assert!(skip.contains(&full), "Full already holds both items");
        assert!(!skip.contains(&half), "Half is still missing one");
        assert!(!skip.contains(&empty), "Empty has room for both");
        assert_eq!(
            *focus,
            SheetField::Idle,
            "there are still collections to choose from, but the search waits for a click"
        );

        app.ask_add_to_collection(vec![first]);
        let Some(Modal::AddToCollection { skip, .. }) = &app.modal else {
            panic!("the collection sheet should be open");
        };
        assert!(skip.contains(&full));
        assert!(skip.contains(&half), "Half already holds this item");
        assert!(!skip.contains(&empty));
    }

    #[test]
    fn the_collection_sheet_draws_the_offered_collections() {
        let (_temp, mut app, ids) = app_with_photos(2);
        let collection = app.library.create_collection("Trip").unwrap();
        app.library
            .add_to_collection(collection, &[ids[0].clone()])
            .unwrap();
        app.reload();
        app.screen = (1440.0, 900.0);

        // Trip does not hold the second photo, so the sheet lists it.
        app.ask_add_to_collection(vec![ids[1].clone()]);
        app.modal_t = 1.0;
        let frame = app.build_frame(1.0 / 60.0);
        assert!(!frame.cmds.is_empty(), "the collection sheet must draw");
    }

    #[test]
    fn the_collection_sheet_offers_every_collection_and_scrolls_the_grid() {
        let (_temp, mut app, ids) = app_with_photos(1);
        for index in 0..30 {
            app.library
                .create_collection(&format!("Trip {index}"))
                .unwrap();
        }
        app.reload();
        app.screen = (1440.0, 900.0);

        // Thirty collections do not fit in the grid at this height, so it
        // scrolls. The sheet offers all thirty, not the first five.
        app.ask_add_to_collection(vec![ids[0].clone()]);
        app.modal_t = 1.0;
        let layout = collection_sheet_layout(1440.0, 900.0, 30, 30);
        assert!(
            layout.height <= 900.0 * 0.8 + 0.01,
            "the sheet stops at four fifths of the window"
        );
        assert!(layout.max_scroll > 0.0, "thirty collections need to scroll");

        // A wheel turn over the grid moves it, and stops at the end.
        app.input.mouse = (720.0, 900.0 - layout.height + layout.head + 40.0);
        app.input.wheel = -10_000.0;
        app.build_frame(1.0 / 60.0);
        assert_eq!(
            app.sheet_scroll, layout.max_scroll,
            "the grid scrolls to its end and no further"
        );

        // Three collections fit on one row, so the grid never scrolls.
        let short = collection_sheet_layout(1440.0, 900.0, 3, 3);
        assert_eq!(short.max_scroll, 0.0);
        assert_eq!(short.grid, SHEET_TILE_H);
        assert_eq!(short.columns, 4, "a sheet this wide fits four tiles across");
    }

    #[test]
    fn the_sheet_searches_collections_and_enter_adds_to_the_first_match() {
        let (_temp, mut app, ids) = app_with_photos(1);
        app.library.create_collection("Holiday 2025").unwrap();
        app.library.create_collection("Work").unwrap();
        app.reload();
        app.ask_add_to_collection(vec![ids[0].clone()]);
        // The search opens closed, so a click on its icon is what gives it the focus.
        click_sheet_search(&mut app);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection {
                    focus: SheetField::Search,
                    ..
                })
            ),
            "with the search clicked, typing searches the collections"
        );

        // Enter with nothing typed must not add to the first collection.
        app.confirm_modal();
        assert!(
            app.modal.is_some(),
            "Enter with no search keeps the sheet open"
        );
        let count = |app: &ArchiveApp, name: &str| {
            app.collections
                .iter()
                .find(|collection| collection.name == name)
                .map(|collection| collection.count)
                .unwrap()
        };
        assert_eq!(count(&app, "Holiday 2025"), 0);

        app.text_input("holi");
        app.confirm_modal();
        assert!(
            app.modal.is_none(),
            "Enter adds to the first match and closes the sheet"
        );
        assert_eq!(count(&app, "Holiday 2025"), 1);
        assert_eq!(count(&app, "Work"), 0);
    }

    #[test]
    fn the_search_ignores_case_and_leaves_out_collections_that_hold_the_selection() {
        let collection = |id: i64, name: &str| Collection {
            id,
            name: name.into(),
            created_at: 0,
            count: 0,
            cover_item_ids: Vec::new(),
            lock: LockState::None,
        };
        let collections = vec![
            collection(1, "Trip to Lisbon"),
            collection(2, "Lisbon work"),
            collection(3, "Cats"),
        ];
        let skip: HashSet<i64> = [2].into_iter().collect();
        let found: Vec<i64> = sheet_offers(&collections, &skip, "  LISBON ")
            .iter()
            .map(|collection| collection.id)
            .collect();
        assert_eq!(
            found,
            vec![1],
            "case does not matter, and the skipped collection is left out"
        );
        assert_eq!(sheet_offers(&collections, &skip, "").len(), 2);
    }

    #[test]
    fn the_name_field_waits_behind_the_plus_and_enter_creates_with_the_selection() {
        let (_temp, mut app, ids) = app_with_photos(1);
        app.library.create_collection("Work").unwrap();
        app.reload();
        app.ask_add_to_collection(vec![ids[0].clone()]);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection {
                    creating: false,
                    ..
                })
            ),
            "with collections to pick from, the name field stays hidden"
        );

        app.start_creating();
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection {
                    creating: true,
                    focus: SheetField::Name,
                    ..
                })
            ),
            "the plus shows the name field and puts the cursor in it"
        );
        app.text_input("Trip");
        app.confirm_modal();
        assert!(
            app.modal.is_none(),
            "Enter creates the collection and closes the sheet"
        );
        let trip = app
            .collections
            .iter()
            .find(|collection| collection.name == "Trip")
            .expect("the collection should exist");
        assert_eq!(trip.count, 1, "the selection goes into the new collection");
    }

    #[test]
    fn the_sheet_search_and_plus_ease_open_and_shut() {
        let (_temp, mut app, ids) = app_with_photos(1);
        app.library.create_collection("Work").unwrap();
        app.reload();
        app.ask_add_to_collection(vec![ids[0].clone()]);

        // Focusing the search starts it opening: it moves, but does not jump.
        app.focus_sheet(SheetField::Search);
        app.animate(1.0 / 60.0);
        let early = app.sheet_search.x;
        assert!(
            early > 0.0 && early < 1.0,
            "the search should be part way open"
        );
        for _ in 0..120 {
            app.animate(1.0 / 60.0);
        }
        assert!(
            app.sheet_search.settled(1.0),
            "the search should settle open"
        );
        assert!(!app.sheet_animating());

        // Starting to create slides the plus over beside the name field.
        app.start_creating();
        for _ in 0..120 {
            app.animate(1.0 / 60.0);
        }
        assert!(
            app.sheet_create.settled(1.0),
            "the plus should settle beside the field"
        );
    }

    #[test]
    fn a_press_outside_a_text_field_takes_the_focus_off_it() {
        let (_temp, mut app, ids) = app_with_photos(1);
        app.library.create_collection("Work").unwrap();
        app.reload();
        app.screen = (1440.0, 900.0);
        app.ask_add_to_collection(vec![ids[0].clone()]);
        // Fully open, so the fields stay where the frame drew them.
        app.modal_t = 1.0;
        app.build_frame(1.0 / 60.0);

        // A press on the sheet's header, clear of its fields, takes the
        // search's focus and nothing else. (A press off the sheet dismisses it.)
        app.input.mouse = (720.0, 670.0);
        app.pointer_button(true);
        app.pointer_button(false);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection {
                    focus: SheetField::Idle,
                    ..
                })
            ),
            "a press off the search field should leave it"
        );
        // Drawing consumes the press, as a frame does in the app.
        app.build_frame(1.0 / 60.0);

        // Creating a collection, then clicking the search field instead: the
        // name field lets go, which cancels creating. Its name is dropped once
        // the field has shut.
        app.start_creating();
        app.text_input("Trip");
        app.build_frame(1.0 / 60.0);
        app.input.mouse = app.sheet_search_rect.center();
        app.pointer_button(true);
        app.pointer_button(false);
        app.build_frame(1.0 / 60.0);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection {
                    creating: false,
                    focus: SheetField::Search,
                    ..
                })
            ),
            "leaving the name field should cancel creating"
        );
        for _ in 0..120 {
            app.animate(1.0 / 60.0);
        }
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection { name, .. }) if name.is_empty()
            ),
            "the name should be dropped once the field has shut"
        );
    }

    #[test]
    fn closing_the_sheet_keeps_the_name_field_until_it_is_gone() {
        let (_temp, mut app, ids) = app_with_photos(1);
        app.ask_add_to_collection(ids);
        app.start_creating();
        app.text_input("Trip");
        app.build_frame(1.0 / 60.0);
        // Pressing Cancel closes the sheet; the name field must not collapse
        // on that press, or the plus lights up for a frame before it leaves.
        app.input.mouse = app.sheet_cancel_rect.center();
        app.blur_text_fields();
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection { creating: true, focus: SheetField::Name, name, .. })
                    if name == "Trip"
            ),
            "a press on Cancel should leave the name field as it is"
        );
    }

    #[test]
    fn leaving_the_name_field_shrinks_it_shut_on_the_spring() {
        let (_temp, mut app, ids) = app_with_photos(1);
        // The search only shows once a collection exists to search.
        app.library.create_collection("Work").unwrap();
        app.reload();
        app.ask_add_to_collection(ids);
        app.start_creating();
        app.text_input("Trip");
        for _ in 0..120 {
            app.animate(1.0 / 60.0);
        }
        assert!(
            app.sheet_create.settled(1.0),
            "the field should open fully first"
        );
        app.build_frame(1.0 / 60.0);
        // Clicking the search field lets the name field go. It must still be
        // on its way shut the next frame, not gone at once.
        app.input.mouse = app.sheet_search_rect.center();
        app.pointer_button(true);
        app.pointer_button(false);
        app.animate(1.0 / 60.0);
        assert!(app.sheet_create.x < 1.0 && app.sheet_create.x > 0.0);
        assert!(app.sheet_animating(), "the field is still shutting");
        app.build_frame(1.0 / 60.0);
        assert_eq!(
            app.sheet_name_rect.w, 0.0,
            "a leaving field takes no presses"
        );
        // The typed name stays on the field while it shuts, as it does open.
        assert!(
            matches!(
                &app.modal,
                Some(Modal::AddToCollection { name, .. }) if name == "Trip"
            ),
            "the name should stay on the field while it shuts"
        );
        for _ in 0..120 {
            app.animate(1.0 / 60.0);
        }
        assert!(app.sheet_create.x < 0.01, "the field ends fully shut");
        assert!(!app.sheet_animating());
    }

    /// Opens an import sheet as `import` would, with no worker behind it.
    fn open_import(app: &mut ArchiveApp) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        app.modal = Some(Modal::Import(ImportSession {
            phase: ImportPhase::Running,
            total: 0,
            done: 0,
            current: String::new(),
            cancel: Arc::clone(&cancel),
            report: None,
            error: None,
            queued: Vec::new(),
            finished_at: None,
        }));
        cancel
    }

    fn import_session(app: &ArchiveApp) -> &ImportSession {
        match &app.modal {
            Some(Modal::Import(session)) => session,
            other => panic!("the import sheet should be open, found {other:?}"),
        }
    }

    /// Distinct photos, so none of them is a duplicate of another.
    fn staged_photos(dir: &std::path::Path, count: usize) -> Vec<PathBuf> {
        std::fs::create_dir_all(dir).unwrap();
        (0..count)
            .map(|index| {
                let mut pixels = vec![128u8; 16 * 16 * 4];
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel[0] = (index * 53 + 1) as u8;
                    pixel[1] = 201;
                    pixel[2] = 17;
                }
                let path = dir.join(format!("staged-{index}.png"));
                std::fs::write(&path, dream::png::encode(16, 16, &pixels)).unwrap();
                path
            })
            .collect()
    }

    fn run_import(app: &ArchiveApp, paths: &[PathBuf]) -> archive_core::ImportReport {
        archive_core::import_paths(
            &app.library,
            paths,
            &archive_core::ImportOptions::default(),
            &mut |_| {},
        )
    }

    /// An import in which one file failed, so the sheet asks for a decision.
    fn run_import_with_problem(app: &ArchiveApp, paths: &[PathBuf]) -> archive_core::ImportReport {
        let mut report = run_import(app, paths);
        report.failed += 1;
        report
            .failures
            .push(("IMG_0.HEIC".into(), "unsupported format".into()));
        report
    }

    #[test]
    fn a_clean_import_completes_and_is_kept_by_itself() {
        let (temp, mut app, _) = app_with_photos(1);
        let photos = staged_photos(&temp.path().join("more"), 3);
        open_import(&mut app);
        app.modal_t = 1.0;
        app.on_import_done(run_import(&app, &photos));
        assert_eq!(import_session(&app).phase, ImportPhase::Complete);
        assert_eq!(app.items.len(), 1, "new media waits for the sheet to close");

        // Still on screen during the hold, then eases out before it closes.
        app.tick_import_sheet();
        assert!(app.modal.is_some());
        app.time += IMPORT_COMPLETE_HOLD;
        app.animate(1.0 / 60.0);
        assert!(
            app.modal_t < 1.0 && app.modal_t > 0.0,
            "the sheet should be easing out"
        );
        app.tick_import_sheet();
        assert!(
            app.modal.is_some(),
            "the sheet closes only once it is out of view"
        );
        for _ in 0..60 {
            app.animate(1.0 / 60.0);
        }
        app.tick_import_sheet();
        assert!(app.modal.is_none());
        assert_eq!(app.items.len(), 4, "the finished import is shown");
    }

    #[test]
    fn a_problem_keeps_the_sheet_open_for_a_decision() {
        let (temp, mut app, _) = app_with_photos(1);
        let photos = staged_photos(&temp.path().join("more"), 3);
        open_import(&mut app);
        app.on_import_done(run_import_with_problem(&app, &photos));
        app.time += 60.0;
        app.tick_import_sheet();
        assert_eq!(import_session(&app).phase, ImportPhase::Review);

        app.confirm_import();
        assert!(app.modal.is_none());
        assert_eq!(app.items.len(), 4, "Confirm keeps the import");
    }

    #[test]
    fn cancel_after_review_takes_the_import_back() {
        let (temp, mut app, _) = app_with_photos(1);
        let photos = staged_photos(&temp.path().join("more"), 3);
        open_import(&mut app);
        app.on_import_done(run_import_with_problem(&app, &photos));
        let ids = import_session(&app)
            .report
            .as_ref()
            .unwrap()
            .created
            .clone();
        assert_eq!(ids.len(), 3);

        app.cancel_import();
        assert_eq!(import_session(&app).phase, ImportPhase::Removing);
        // The worker does this off the UI thread; here it runs inline.
        app.library.delete_forever(&ids).unwrap();
        app.on_import_discarded(Ok(()));
        assert!(app.modal.is_none());
        assert_eq!(
            app.library.item_count(&TimelineFilter::default()).unwrap(),
            1,
            "only the photo that was already archived remains"
        );
    }

    #[test]
    fn cancel_mid_copy_stops_and_takes_back_what_landed() {
        let (temp, mut app, _) = app_with_photos(1);
        let photos = staged_photos(&temp.path().join("more"), 3);
        let cancel = open_import(&mut app);
        app.cancel_import();
        assert_eq!(import_session(&app).phase, ImportPhase::Stopping);
        assert!(
            cancel.load(Ordering::Relaxed),
            "Cancel raises the flag the copy watches"
        );

        // Rows that landed before the copy stopped are removed as it stops.
        let report = run_import(&app, &photos);
        let ids = report.created.clone();
        app.on_import_done(report);
        assert_eq!(import_session(&app).phase, ImportPhase::Removing);
        app.library.delete_forever(&ids).unwrap();
        app.on_import_discarded(Ok(()));
        assert!(app.modal.is_none());
        assert_eq!(
            app.library.item_count(&TimelineFilter::default()).unwrap(),
            1
        );
    }

    #[test]
    fn a_failed_take_back_stays_open_to_try_again() {
        let (temp, mut app, _) = app_with_photos(1);
        let photos = staged_photos(&temp.path().join("more"), 2);
        open_import(&mut app);
        app.on_import_done(run_import_with_problem(&app, &photos));
        app.cancel_import();
        app.on_import_discarded(Err("the disk is full".into()));

        let session = import_session(&app);
        assert_eq!(session.phase, ImportPhase::Review);
        assert!(
            session
                .error
                .as_deref()
                .is_some_and(|error| error.contains("the disk is full")),
            "{:?}",
            session.error
        );
    }

    #[test]
    fn files_dropped_while_the_sheet_is_open_wait_for_it() {
        let (temp, mut app, _) = app_with_photos(1);
        let first = staged_photos(&temp.path().join("first"), 2);
        let later = staged_photos(&temp.path().join("later"), 1);
        open_import(&mut app);
        app.import(later.clone());
        assert_eq!(import_session(&app).queued, later);

        app.on_import_done(run_import(&app, &first));
        app.confirm_import();
        let session = import_session(&app);
        assert_eq!(
            session.phase,
            ImportPhase::Running,
            "the dropped files start on their own sheet"
        );
        assert!(session.queued.is_empty());
    }

    #[test]
    fn removing_from_a_collection_keeps_the_item() {
        let (_temp, mut app, ids) = app_with_photos(1);
        let id = ids[0].clone();
        let collection_id = app.library.create_collection("Trip").unwrap();
        app.library
            .add_to_collection(collection_id, std::slice::from_ref(&id))
            .unwrap();
        app.set_page(Page::Collection(collection_id));
        assert_eq!(app.display.len(), 1);

        app.select_mode = true;
        app.selection.insert(id.clone());
        app.remove_from_album(app.selection_ids());

        assert!(app.selection.is_empty());
        assert!(
            app.library.item(&id).unwrap().is_some(),
            "the item must survive leaving a collection"
        );
        assert!(
            app.display.is_empty(),
            "the collection must no longer list the item"
        );
        assert_eq!(
            app.collections
                .iter()
                .find(|album| album.id == collection_id)
                .unwrap()
                .count,
            0
        );
    }

    #[test]
    fn collections_search_matches_names_and_images_inside_them() {
        let (_temp, mut app, ids) = app_with_photos(2);
        let trip = app.library.create_collection("Trip").unwrap();
        let work = app.library.create_collection("Work").unwrap();
        app.library
            .add_to_collection(trip, &[ids[0].clone()])
            .unwrap();
        app.library
            .add_to_collection(work, &[ids[1].clone()])
            .unwrap();
        app.set_page(Page::Collections);

        let shown = |app: &ArchiveApp| {
            app.cover_tiles
                .iter()
                .map(|tile| tile.page.clone())
                .collect::<Vec<_>>()
        };

        app.search = "tri".into();
        app.filter_display();
        assert_eq!(
            shown(&app),
            vec![Page::Collection(trip)],
            "the collection name matches"
        );

        // A semantic hit for an image inside Work lifts Work, not Trip.
        app.search = "a red square".into();
        app.search_hits = Some(("a red square".into(), vec![ids[1].clone()]));
        app.filter_display();
        assert_eq!(
            shown(&app),
            vec![Page::Collection(work)],
            "the collection containing the matching image matches"
        );

        // The name still matches even with no semantic results.
        app.search_hits = None;
        app.search = "work".into();
        app.filter_display();
        assert_eq!(shown(&app), vec![Page::Collection(work)]);
    }

    /// Songs with an album tag are sorted into albums by the library. An album
    /// is listed while any of its songs is, and goes once the last is trashed.
    #[test]
    fn music_albums_follow_the_album_tags_of_the_songs_in_the_library() {
        let Some(ffmpeg) = archive_core::Tools::discover().ffmpeg else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        let paths = write_songs(
            &ffmpeg,
            &source,
            &[
                (
                    "one.mp3",
                    1,
                    &["album=Record", "album_artist=Band", "track=1"],
                ),
                (
                    "two.mp3",
                    1,
                    &["album=Record", "album_artist=Band", "track=2"],
                ),
                ("loose.mp3", 1, &["title=Loose", "track=1", "genre=Test"]),
            ],
        );
        let library_root = temp.path().join("library");
        let settings = Settings {
            library_root: Some(library_root.clone()),
            ..Default::default()
        };
        let mut app = ArchiveApp::headless(library_root, settings).unwrap();
        let report = run_import(&app, &paths);
        assert_eq!(report.imported, 3, "the fixture should import");

        app.set_page(Page::Albums);
        assert_eq!(app.albums.len(), 1, "the untagged song makes no album");
        let album = app.albums[0].clone();
        assert_eq!((album.name.as_str(), album.count), ("Record", 2));
        let tiles: Vec<Page> = app
            .cover_tiles
            .iter()
            .map(|tile| tile.page.clone())
            .collect();
        assert_eq!(tiles, vec![Page::Album(album.key.clone())]);

        // Opening the album lists its two songs and nothing else.
        app.set_page(Page::Album(album.key.clone()));
        assert_eq!(app.display.len(), 2);
        assert!(
            app.display
                .iter()
                .all(|item| item.original_name != "loose.mp3")
        );

        // Trashing both songs leaves the album empty, so it goes and its page
        // closes back to the list.
        let ids: Vec<String> = app.display.iter().map(|item| item.id.clone()).collect();
        app.library.trash(&ids).unwrap();
        app.reload();
        assert!(app.albums.is_empty());
        assert_eq!(app.page, Page::Albums);
        assert!(app.cover_tiles.is_empty());
    }

    /// Writes one tone per song, `seconds` long, with the given ffmpeg tags.
    fn write_songs(
        ffmpeg: &std::path::Path,
        dir: &std::path::Path,
        songs: &[(&str, u32, &[&str])],
    ) -> Vec<std::path::PathBuf> {
        std::fs::create_dir_all(dir).unwrap();
        songs
            .iter()
            .map(|(name, seconds, tags)| {
                let path = dir.join(name);
                let duration = format!("sine=frequency=440:duration={seconds}");
                let mut args = vec![
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    duration.as_str(),
                    "-c:a",
                    "libmp3lame",
                    "-b:a",
                    "128k",
                ];
                for tag in tags.iter().copied() {
                    args.extend(["-metadata", tag]);
                }
                args.push(path.to_str().unwrap());
                let status = std::process::Command::new(ffmpeg)
                    .args(&args)
                    .status()
                    .unwrap();
                assert!(status.success(), "ffmpeg should write {name}");
                path
            })
            .collect()
    }

    /// On the Media page the songs of an album that has several show as one
    /// tile: the first song stands for the album, with its whole run time. The
    /// album's page lists every song, and a lone song keeps its own tile.
    #[test]
    fn media_shows_an_album_of_several_songs_as_one_tile() {
        let Some(ffmpeg) = archive_core::Tools::discover().ffmpeg else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let temp = tempfile::TempDir::new().unwrap();
        let paths = write_songs(
            &ffmpeg,
            &temp.path().join("source"),
            &[
                (
                    "one.mp3",
                    1,
                    &["album=Record", "album_artist=Band", "track=1"],
                ),
                (
                    "two.mp3",
                    2,
                    &["album=Record", "album_artist=Band", "track=2"],
                ),
                ("loose.mp3", 1, &["title=Loose", "track=1", "genre=Test"]),
            ],
        );
        let library_root = temp.path().join("library");
        let settings = Settings {
            library_root: Some(library_root.clone()),
            ..Default::default()
        };
        let mut app = ArchiveApp::headless(library_root, settings).unwrap();
        assert_eq!(run_import(&app, &paths).imported, 3);

        app.reload();
        assert_eq!(app.display.len(), 2, "two songs of the album make one tile");
        let loose = app
            .display
            .iter()
            .find(|item| item.original_name == "loose.mp3")
            .expect("the lone song keeps its tile")
            .clone();
        assert!(app.media_album(&loose).is_none());
        let tile = app
            .display
            .iter()
            .find(|item| item.original_name != "loose.mp3")
            .expect("the album's tile")
            .clone();
        let album = app
            .media_album(&tile)
            .expect("the tile is the album")
            .clone();
        assert_eq!(album.count, 2);
        assert!(
            (album.duration_ms - 3000).abs() < 300,
            "the tile shows the album's total run time, got {}",
            album.duration_ms
        );

        // Scrolling in or out of the timeline never brings the hidden songs back.
        app.filter_display();
        assert_eq!(app.display.len(), 2);

        app.set_page(Page::Album(album.key.clone()));
        assert_eq!(app.display.len(), 2, "the album page lists both songs");
    }

    /// The back arrow of an album returns to the Media page where it was left,
    /// but an album opened from the Albums list still returns to that list.
    #[test]
    fn the_album_back_arrow_returns_to_media_where_it_was() {
        let Some(ffmpeg) = archive_core::Tools::discover().ffmpeg else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let temp = tempfile::TempDir::new().unwrap();
        let paths = write_songs(
            &ffmpeg,
            &temp.path().join("source"),
            &[
                (
                    "one.mp3",
                    1,
                    &["album=Record", "album_artist=Band", "track=1"],
                ),
                (
                    "two.mp3",
                    2,
                    &["album=Record", "album_artist=Band", "track=2"],
                ),
                ("loose.mp3", 1, &["title=Loose", "track=1", "genre=Test"]),
            ],
        );
        let library_root = temp.path().join("library");
        let settings = Settings {
            library_root: Some(library_root.clone()),
            ..Default::default()
        };
        let mut app = ArchiveApp::headless(library_root, settings).unwrap();
        assert_eq!(run_import(&app, &paths).imported, 3);
        app.reload();
        let key = app.albums[0].key.clone();

        app.scroll_target = 40.0;
        app.set_page(Page::Album(key.clone()));
        app.leave_open_page();
        assert_eq!(app.page, Page::Media, "the back arrow returns to Media");
        assert_eq!(app.scroll_target, 40.0, "at the same scroll position");
        assert_eq!(app.display.len(), 2);

        app.set_page(Page::Albums);
        app.set_page(Page::Album(key));
        app.leave_open_page();
        assert_eq!(
            app.page,
            Page::Albums,
            "from the list it still goes to the list"
        );
    }

    fn actions_of(rows: &[MenuRow]) -> Vec<MenuAction> {
        rows.iter()
            .filter_map(|row| match row {
                MenuRow::Item { action, .. } => Some(action.clone()),
                _ => None,
            })
            .collect()
    }

    fn menu_actions(app: &ArchiveApp) -> Vec<MenuAction> {
        app.menu
            .as_ref()
            .map(|menu| actions_of(&menu.rows))
            .unwrap_or_default()
    }

    fn menu_headers(app: &ArchiveApp) -> Vec<String> {
        app.menu
            .as_ref()
            .map(|menu| {
                menu.rows
                    .iter()
                    .filter_map(|row| match row {
                        MenuRow::Header(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Where a tile of the media grid is drawn at rest, on a 1440 by 900 window.
    fn tile_point(app: &ArchiveApp, index: usize) -> (f32, f32) {
        let tile = app
            .grid
            .tiles
            .iter()
            .find(|tile| tile.index == index)
            .expect("the tile should be laid out");
        (
            crate::theme::SIDEBAR_W + 24.0 + tile.rect.x + tile.rect.w * 0.5,
            crate::theme::TOPBAR_H + 10.0 + tile.rect.y + tile.rect.h * 0.5 - app.scroll,
        )
    }

    /// Lays the page out, then right-clicks the tile at `index`, and lets the
    /// menu finish opening.
    fn right_click_tile(app: &mut ArchiveApp, index: usize) {
        app.screen = (1440.0, 900.0);
        app.build_frame(1.0 / 60.0);
        app.input.mouse = tile_point(app, index);
        app.pointer_secondary();
        app.build_frame(1.0 / 60.0);
        for _ in 0..60 {
            app.animate(1.0 / 60.0);
        }
    }

    #[test]
    fn a_right_click_on_a_tile_offers_its_actions_for_that_tile_alone() {
        let (_temp, mut app, _ids) = app_with_photos(3);
        let target = app.display[1].id.clone();
        right_click_tile(&mut app, 1);

        assert!(app.menu_open(), "the right-click should open the menu");
        assert!(
            app.selection.is_empty(),
            "an unselected tile is acted on alone, and nothing is selected"
        );
        let actions = menu_actions(&app);
        assert!(actions.contains(&MenuAction::AddToCollection(vec![target.clone()])));
        assert!(actions.contains(&MenuAction::Trash(vec![target])));
        assert!(
            menu_headers(&app).is_empty(),
            "one item needs no count above its rows"
        );
    }

    #[test]
    fn a_right_click_on_a_selected_tile_acts_on_the_whole_selection() {
        let (_temp, mut app, _ids) = app_with_photos(3);
        app.screen = (1440.0, 900.0);
        app.build_frame(1.0 / 60.0);
        let picked: Vec<String> = app
            .display
            .iter()
            .take(2)
            .map(|item| item.id.clone())
            .collect();
        app.toggle_selection(&picked[0], 0);
        app.toggle_selection(&picked[1], 1);

        right_click_tile(&mut app, 1);

        assert_eq!(menu_headers(&app), vec!["2 selected".to_string()]);
        let mut expected = picked;
        expected.sort();
        assert!(menu_actions(&app).contains(&MenuAction::Trash(expected)));
        assert_eq!(app.selection.len(), 2, "the selection stays as it was");
    }

    #[test]
    fn the_trash_row_moves_only_the_items_the_menu_acts_on() {
        let (_temp, mut app, _ids) = app_with_photos(3);
        let target = app.display[0].id.clone();
        let bystander = app.display[2].id.clone();
        right_click_tile(&mut app, 0);

        let trash = menu_actions(&app)
            .into_iter()
            .find(|action| matches!(action, MenuAction::Trash(_)))
            .expect("the menu should offer trash");
        app.run_menu_action(trash);
        app.confirm_modal();

        assert!(
            app.library
                .item(&target)
                .unwrap()
                .unwrap()
                .deleted_at
                .is_some(),
            "the item the menu was opened on goes to the trash"
        );
        assert!(
            app.library
                .item(&bystander)
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none(),
            "the other items stay where they are"
        );
    }

    #[test]
    fn escape_shuts_the_menu_by_fading_it_and_acts_on_nothing() {
        let (_temp, mut app, _ids) = app_with_photos(2);
        right_click_tile(&mut app, 0);
        assert!(app.menu_open());

        app.key(dream::ui::widgets::Key::Escape, true);
        assert!(
            app.menu.as_ref().is_some_and(|menu| menu.closing),
            "Escape starts the fade"
        );
        app.animate(1.0 / 60.0);
        assert!(
            app.menu.is_some(),
            "the menu is still drawn for the first frames of its fade"
        );
        for _ in 0..120 {
            app.animate(1.0 / 60.0);
        }
        assert!(app.menu.is_none(), "the faded menu is dropped");
        assert!(app.modal.is_none() && app.viewer.is_none());
    }

    #[test]
    fn a_press_off_the_card_dismisses_the_menu_and_reaches_nothing_beneath() {
        let (_temp, mut app, _ids) = app_with_photos(2);
        right_click_tile(&mut app, 0);
        let far = (app.screen.0 - 24.0, app.screen.1 - 24.0);
        assert!(
            !app.menu_rect.contains(far),
            "the test point is off the card"
        );

        app.input.mouse = far;
        app.pointer_button(true);
        app.build_frame(1.0 / 60.0);
        app.pointer_button(false);
        app.build_frame(1.0 / 60.0);

        assert!(
            app.menu.as_ref().is_some_and(|menu| menu.closing),
            "a press off the card dismisses it"
        );
        assert!(app.modal.is_none());
        assert!(app.viewer.is_none(), "the press did not reach the grid");
    }

    #[test]
    fn a_right_click_on_the_open_menu_is_its_own_and_opens_nothing() {
        let (_temp, mut app, _ids) = app_with_photos(2);
        right_click_tile(&mut app, 0);
        app.input.mouse = app.menu_rect.center();
        app.pointer_secondary();
        assert!(
            !app.input.secondary_pressed,
            "a press on the menu is not passed on to what it covers"
        );
    }

    #[test]
    fn the_arrow_keys_move_through_the_rows_and_wrap_around() {
        let (_temp, mut app, _ids) = app_with_photos(1);
        right_click_tile(&mut app, 0);
        // Add to collection, then Move to trash.
        let focus = |app: &ArchiveApp| app.menu.as_ref().and_then(|menu| menu.focus);

        app.key(dream::ui::widgets::Key::Down, true);
        assert_eq!(focus(&app), Some(0));
        app.key(dream::ui::widgets::Key::Down, true);
        assert_eq!(focus(&app), Some(1));
        app.key(dream::ui::widgets::Key::Down, true);
        assert_eq!(focus(&app), Some(0), "Down wraps to the first row");
        app.key(dream::ui::widgets::Key::Up, true);
        assert_eq!(focus(&app), Some(1), "Up wraps to the last row");
    }

    #[test]
    fn a_collection_menu_offers_the_ways_its_lock_allows_and_an_album_offers_nothing() {
        assert_eq!(
            actions_of(&ArchiveApp::collection_menu(
                &Page::Collection(7),
                LockState::None
            )),
            vec![MenuAction::StartLock(7), MenuAction::DeleteCollection(7)]
        );
        // A locked collection cannot be deleted: its items would be lost with it.
        assert_eq!(
            actions_of(&ArchiveApp::collection_menu(
                &Page::Collection(7),
                LockState::Locked
            )),
            vec![MenuAction::StartUnlock(7), MenuAction::StartRemoveLock(7)]
        );
        assert_eq!(
            actions_of(&ArchiveApp::collection_menu(
                &Page::Collection(7),
                LockState::Unlocked
            )),
            vec![
                MenuAction::LockNow(7),
                MenuAction::StartRemoveLock(7),
                MenuAction::DeleteCollection(7)
            ]
        );
        assert!(
            ArchiveApp::collection_menu(&Page::Album("key".into()), LockState::None).is_empty()
        );
    }

    #[test]
    fn an_unlocked_collection_locks_a_minute_after_the_last_input_and_no_sooner() {
        let (_temp, mut app, _ids) = app_with_photos(1);
        assert!(
            app.vault_wake().is_none(),
            "nothing is unlocked, so nothing is timed"
        );
        app.collections = vec![Collection {
            id: 1,
            name: "Private".into(),
            created_at: 0,
            count: 1,
            cover_item_ids: Vec::new(),
            lock: LockState::Unlocked,
        }];
        app.last_activity = Instant::now() - Duration::from_secs(30);
        let deadline = app.vault_wake().expect("an unlocked collection is timed");
        assert!(
            deadline > Instant::now(),
            "30 seconds of quiet is not a minute"
        );
        assert_eq!(deadline, app.last_activity + VAULT_IDLE);

        // With nothing open on the collection's page, the timer runs there too.
        app.page = Page::Collection(1);
        app.viewer = None;
        assert!(app.vault_wake().is_some());
        app.vault_busy = true;
        assert!(
            app.vault_wake().is_none(),
            "a job in progress is not timed again"
        );
    }

    /// Waits for a background vault job to report, feeding its result to the app.
    fn wait_for_vault(app: &mut ArchiveApp) {
        let started = Instant::now();
        while app.vault_busy {
            app.drain_events();
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "the vault job did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn password_error(app: &ArchiveApp) -> Option<String> {
        match &app.modal {
            Some(Modal::Password(session)) => session.error.clone(),
            _ => None,
        }
    }

    #[test]
    fn a_texture_asked_for_before_a_lock_is_dropped_not_shown() {
        let (_temp, mut app, ids) = app_with_photos(2);
        let (stale, current) = (ids[0].clone(), ids[1].clone());
        let failed = |id: &str, epoch: u64| super::ReadyTexture {
            key: crate::worker::TexKind::Thumb.key(id),
            id: id.to_string(),
            kind: crate::worker::TexKind::Thumb,
            epoch,
            result: Err("decode failed".into()),
        };
        // The decode is asked for, then a collection is sealed before it arrives.
        app.grid_textures.push_back(failed(&stale, app.vault_epoch));
        app.collections_sealed();
        app.upload_ready_textures();
        assert!(
            !app.thumb_failed.contains(&stale),
            "a pre-lock result was applied"
        );
        assert!(app.grid_textures.is_empty());

        // A result asked under the current epoch is applied as usual.
        app.grid_textures
            .push_back(failed(&current, app.vault_epoch));
        app.upload_ready_textures();
        assert!(app.thumb_failed.contains(&current));
    }

    #[test]
    fn a_password_lock_is_made_opened_relocked_and_hidden_through_the_dialog() {
        let (_temp, mut app, ids) = app_with_photos(3);
        let private = app.library.create_collection("Private").unwrap();
        app.library.add_to_collection(private, &ids[..2]).unwrap();
        app.reload();
        assert_eq!(app.items.len(), 3);

        // Creating a lock asks for the password twice, and checks the first.
        app.ask_password(private, PasswordMode::Create);
        app.text_input("short");
        app.submit_password();
        assert!(password_error(&app).unwrap().contains("Use at least"));
        assert!(!app.vault_busy, "nothing is started for a refused password");

        app.text_input("ong"); // "shortong": long enough, but not yet the confirmation
        app.key(dream::ui::widgets::Key::Tab, true);
        app.text_input("shortong");
        app.submit_password();
        assert!(app.vault_busy, "matching passwords start the lock");
        wait_for_vault(&mut app);
        assert!(
            app.modal.is_none(),
            "the dialog closes once the collection is locked"
        );
        assert_eq!(app.collection_lock(private), LockState::Locked);
        assert!(
            app.vault_epoch > 0,
            "sealing a collection moves the epoch on"
        );
        assert_eq!(
            app.items.len(),
            1,
            "only the photo outside the lock is visible"
        );

        // A wrong password keeps the dialog open and says so.
        app.press_vault_button(private);
        app.text_input("not the password");
        app.submit_password();
        wait_for_vault(&mut app);
        assert!(password_error(&app).unwrap().contains("does not open"));
        assert_eq!(app.collection_lock(private), LockState::Locked);

        // The right one opens it, and its photos are then on its own page.
        if let Some(Modal::Password(session)) = app.modal.as_mut() {
            session.password.clear();
        }
        app.text_input("shortong");
        app.submit_password();
        wait_for_vault(&mut app);
        assert!(app.modal.is_none());
        assert_eq!(app.collection_lock(private), LockState::Unlocked);
        assert_eq!(app.items.len(), 1);
        app.set_page(Page::Collection(private));
        assert_eq!(app.items.len(), 2, "the lock's photos show inside it");

        // Idle for a minute locks it again, without any key press.
        app.last_activity = Instant::now() - VAULT_IDLE;
        app.vault_idle_check(Instant::now());
        wait_for_vault(&mut app);
        assert_eq!(app.collection_lock(private), LockState::Locked);
        assert!(app.items.is_empty(), "a locked collection shows nothing");
    }

    #[test]
    fn the_trash_menu_restores_or_deletes_for_good() {
        let (_temp, mut app, _ids) = app_with_photos(2);
        let id = app.display[0].id.clone();
        app.library.trash(std::slice::from_ref(&id)).unwrap();
        app.set_page(Page::Trash);

        let actions = actions_of(&app.tile_menu(&id));
        assert!(actions.contains(&MenuAction::Restore(vec![id.clone()])));
        assert!(actions.contains(&MenuAction::DeleteForever(vec![id.clone()])));
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                MenuAction::Trash(_) | MenuAction::AddToCollection(_)
            )),
            "trashed items are not trashed again or added to collections"
        );
    }

    #[test]
    fn the_viewer_menu_offers_details_collections_and_trash() {
        let (_temp, mut app, _ids) = app_with_photos(2);
        let id = app.display[0].id.clone();
        app.open_viewer(0);

        let actions = actions_of(&app.viewer_menu());
        assert!(actions.contains(&MenuAction::ToggleDetails));
        assert!(actions.contains(&MenuAction::AddToCollection(vec![id.clone()])));
        assert!(actions.contains(&MenuAction::Trash(vec![id])));
    }

    #[test]
    fn a_menu_opens_toward_the_pointer_and_turns_back_at_the_edges() {
        let screen = (1440.0, 900.0);
        let size = (220.0, 200.0);

        let inside = menu_place((300.0, 300.0), size, screen);
        assert_eq!((inside.x, inside.y), (300.0, 300.0));

        let corner = menu_place((1430.0, 890.0), size, screen);
        assert!(
            corner.max().0 <= screen.0 - 8.0 && corner.max().1 <= screen.1 - 8.0,
            "the card stays on screen: {corner:?}"
        );
        assert!(
            corner.x < 1430.0 && corner.y < 890.0,
            "the card turns back toward the pointer: {corner:?}"
        );
    }
}
