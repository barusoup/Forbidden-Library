//! Forbidden Library's engine.
//!
//! The library is a directory tree owned by the app:
//!
//! ```text
//! <root>/
//!   library.db          SQLite catalogue (WAL)
//!   store/ab/cd/<hash>  content-addressed originals
//!   thumbs/ab/<id>_384  cached grid previews
//!   thumbs/ab/<id>_2048 cached viewer previews
//!   tmp/                staging only; wiped at every open
//!   logs/
//! ```
//!
//! Every import copies into `tmp`, hashes the bytes while copying, fsyncs,
//! and only then moves the file into the store and commits the catalogue
//! row. A crash can leave an orphaned store file (harmless, reported by
//! `verify`), but never a half-written archive file.

mod anim;
mod audio;
mod crypto;
mod db;
mod decode;
mod error;
mod ffmpeg;
mod import;
mod jxl;
mod locked;
mod lyrics;
mod model;
mod optimize;
mod paths;
mod playback;
mod probe;
mod semantic;
mod store;
mod thumb;
mod time;
mod verify;

pub use anim::{AnimFrame, frames, is_animated};
pub use audio::{Meta as AudioMeta, Tags as AudioTags};
pub use crypto::MIN_PASSWORD_CHARS;
pub use db::{ItemPage, PageCursor};
pub use error::{Error, Result};
pub use ffmpeg::{Stream, Tools, VideoOut, spawn_audio, spawn_video};
pub use import::{ImportEvent, ImportOptions, import_paths};
pub use lyrics::{Line as LyricLine, current as current_lyric, parse as parse_lyrics};
pub use model::BackupReport;
pub use model::{
    Album, COVER_LAYERS, Collection, ImportReport, Item, Kind, LockState, OptState, Stats,
    ThumbState, TimelineFilter, VerifyReport, album_key, albums,
};
pub use optimize::OptimizeConfig;
pub use paths::Paths;
pub use playback::{PlaybackInfo, fit_even, playback_info, playback_info_from_json};
pub use probe::{MediaInfo, inspect};
pub use semantic::{EMBED_DIMS, EMBED_MODEL, VectorIndex, prepare};
pub use thumb::{FULL_SIDE, RECIPE as THUMB_RECIPE, THUMB_SIDE, VIEWER_SIDE};
pub use verify::VerifyOptions;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockWriteGuard};

/// Takes the file gate for writing. A poisoned gate is still usable: it only
/// orders file work, and no data hangs off it.
pub(crate) fn write_gate(gate: &RwLock<()>) -> RwLockWriteGuard<'_, ()> {
    gate.write().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// How long a trashed item is kept before it is deleted permanently.
pub const TRASH_RETENTION_DAYS: i64 = 30;

const DAY_MS: i64 = 86_400_000;

/// Whole days an item stamped `deleted_at` has left in the trash, rounded
/// up. Zero means it is due to be deleted now.
pub fn trash_days_left(deleted_at: i64, now: i64) -> i64 {
    let expires = deleted_at.saturating_add(TRASH_RETENTION_DAYS * DAY_MS);
    let remaining = expires.saturating_sub(now);
    if remaining <= 0 {
        return 0;
    }
    let left = (remaining - 1) / DAY_MS + 1;
    left.min(TRASH_RETENTION_DAYS)
}

/// The countdown shown on trashed items, e.g. "12 days left".
pub fn trash_days_label(deleted_at: i64) -> String {
    match trash_days_left(deleted_at, time::now_ms()) {
        0 => "Deleting today".into(),
        1 => "1 day left".into(),
        days => format!("{days} days left"),
    }
}

/// The archive catalogue and its files.
pub struct Library {
    pub(crate) paths: Paths,
    db: Mutex<rusqlite::Connection>,
    // File mutations share this gate; backups require a quiescent store.
    pub(crate) files: RwLock<()>,
    // The data key of each collection unlocked in this session. A key lives
    // here and nowhere else, and is dropped (zeroed) when its collection locks.
    pub(crate) vault_keys: Mutex<HashMap<i64, crypto::Key>>,
    /// Store files installed by an import whose rows are not committed yet.
    /// Shared by every import, so none deletes bytes another still needs.
    pub(crate) in_flight: import::InFlight,
    /// Held for the life of the process. Two instances sharing one library
    /// would wipe each other's staging files and interleave writes, so the
    /// second one is refused instead.
    _lock: File,
}

impl Library {
    /// Open (or create) the archive at `root`. Staging leftovers in `tmp/`
    /// are wiped, and trashed items past the retention window are deleted;
    /// nothing else in the store or catalogue is touched by opening.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let paths = Paths::new(root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.root().join("library.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::Msg(
                    "this library is already open in another Forbidden Library window".into(),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let conn = db::open(paths.db())?;
        let library = Self {
            paths,
            db: Mutex::new(conn),
            files: RwLock::new(()),
            vault_keys: Mutex::new(HashMap::new()),
            in_flight: import::InFlight::default(),
            _lock: lock,
        };
        library.clean_tmp()?;
        // Plaintext left behind by an interrupted lock or unlock goes now, and
        // sealed folders no lock refers to are removed. Neither blocks opening.
        let _ = library.reconcile_vault();
        // Items left in the trash past the retention window go for good. A
        // failed sweep must never keep the library from opening; the next
        // open (or the next visit to Trash) tries again.
        let _ = library.purge_expired_trash();
        Ok(library)
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn root(&self) -> &Path {
        self.paths.root()
    }

    pub(crate) fn conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.db.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Wipe staging leftovers from a previous run. A file there was never
    /// committed, so it is always safe to remove.
    fn clean_tmp(&self) -> Result<()> {
        let tmp = self.paths.tmp();
        if !tmp.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(tmp)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    }

    // --- Catalogue reads -------------------------------------------------

    /// The music details of an item, or `None` when it is not a music file.
    pub fn audio_meta(&self, id: &str) -> Result<Option<AudioMeta>> {
        db::audio_meta(&self.conn(), id)
    }

    /// The synced lyrics of a song as LRC text, or `None` when none were
    /// imported with it. Parse with [`parse_lyrics`].
    pub fn lyrics(&self, id: &str) -> Result<Option<String>> {
        db::lyrics(&self.conn(), id)
    }

    /// Every music item in album order, each with its tags. See
    /// [`db::music_library`] for the ordering.
    pub fn music(&self) -> Result<Vec<(Item, Option<AudioMeta>)>> {
        db::music_library(&self.conn())
    }

    pub fn item(&self, id: &str) -> Result<Option<Item>> {
        db::get_item(&self.conn(), id)
    }

    /// The filter as this session may read it. A locked collection that is not
    /// unlocked matches nothing, even if rows for it are still in the catalogue.
    fn readable(&self, filter: &TimelineFilter) -> TimelineFilter {
        let mut filter = filter.clone();
        if let Some(id) = filter.collection_id
            && self.lock_state(id).unwrap_or(LockState::None) == LockState::Locked
        {
            filter.none = true;
        }
        filter
    }

    pub fn items(&self, filter: &TimelineFilter) -> Result<Vec<Item>> {
        let filter = self.readable(filter);
        db::list_items(&self.conn(), &filter)
    }

    /// One bounded page of the timeline. `after` is the cursor of the previous
    /// page; `None` starts at the top, and only that first page carries the
    /// total the scrollbar needs.
    pub fn item_page(
        &self,
        filter: &TimelineFilter,
        after: Option<&PageCursor>,
        limit: usize,
    ) -> Result<ItemPage> {
        let filter = self.readable(filter);
        db::item_page(&self.conn(), &filter, after, limit)
    }

    pub fn item_count(&self, filter: &TimelineFilter) -> Result<usize> {
        let filter = self.readable(filter);
        db::count_items(&self.conn(), &filter)
    }

    /// Every id in the filtered timeline, for select-all.
    pub fn item_ids(&self, filter: &TimelineFilter) -> Result<Vec<String>> {
        let filter = self.readable(filter);
        db::item_ids(&self.conn(), &filter)
    }

    /// `(id, captured_at)` for every item in the filter, for date searches
    /// that must span the whole library without loading it.
    pub fn capture_index(&self, filter: &TimelineFilter) -> Result<Vec<(String, i64)>> {
        let filter = self.readable(filter);
        db::capture_index(&self.conn(), &filter)
    }

    /// Catalogue totals. Reads SQL only: the interface calls this on every
    /// load, so it never touches the filesystem.
    pub fn stats(&self) -> Result<Stats> {
        db::stats(&self.conn())
    }

    /// Totals plus a stat per stored file and preview. Only the verifier
    /// and the Utilities page ask for this.
    pub fn stats_with_files(&self) -> Result<Stats> {
        db::stats_with_files(&self.conn(), &self.paths)
    }

    /// Several catalogue rows in one query.
    pub fn items_by_ids(&self, ids: &[String]) -> Result<Vec<Item>> {
        db::items_by_ids(&self.conn(), ids)
    }

    pub fn collections(&self) -> Result<Vec<Collection>> {
        let mut collections = db::collections(&self.conn())?;
        for collection in &mut collections {
            if collection.lock == LockState::None || self.is_unlocked(collection.id) {
                continue;
            }
            // The rows of a collection can outlive a crash that left it unlocked on
            // disk. Without this session's key they stay hidden, and nothing about
            // them is counted or shown.
            collection.lock = LockState::Locked;
            collection.count = 0;
            collection.cover_item_ids.clear();
        }
        Ok(collections)
    }

    pub fn create_collection(&self, name: &str) -> Result<i64> {
        db::create_collection(&self.conn(), name)
    }

    /// Delete a collection. Its photos and videos stay in the library. A locked
    /// collection must be unlocked first, because its items can only be returned
    /// to the library by reading them.
    pub fn delete_collection(&self, id: i64) -> Result<()> {
        let _files = write_gate(&self.files);
        match self.lock_state(id)? {
            LockState::Locked => {
                return Err(Error::msg("unlock this collection before deleting it"));
            }
            LockState::Unlocked => self.remove_lock_unlocked(id)?,
            LockState::None => {}
        }
        db::delete_collection(&self.conn(), id)
    }

    pub fn remove_from_collection(&self, collection_id: i64, ids: &[String]) -> Result<usize> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        if self.lock_state(collection_id)? == LockState::Locked {
            return Err(Error::msg("unlock this collection first"));
        }
        db::remove_from_collection(&self.conn(), collection_id, ids)
    }

    /// Collections that hold at least one of `ids`.
    pub fn collection_ids_for_items(&self, ids: &[String]) -> Result<Vec<i64>> {
        db::collection_ids_for_items(&self.conn(), ids)
    }

    /// Collections that already hold every one of `ids`.
    pub fn collections_containing_all(&self, ids: &[String]) -> Result<Vec<i64>> {
        db::collections_containing_all(&self.conn(), ids)
    }

    /// Add items to a collection. Adding to a locked collection takes the items
    /// out of every other collection and off every page, so it needs the
    /// collection unlocked.
    pub fn add_to_collection(&self, collection_id: i64, ids: &[String]) -> Result<usize> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        match self.lock_state(collection_id)? {
            LockState::Locked => Err(Error::msg("unlock this collection to add to it")),
            LockState::Unlocked => db::add_to_locked(&self.conn(), collection_id, ids),
            LockState::None => db::add_to_collection(&self.conn(), collection_id, ids),
        }
    }

    pub fn set_thumb_state(
        &self,
        id: &str,
        state: ThumbState,
        avg_color: Option<u32>,
    ) -> Result<()> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        db::set_thumb_state(&self.conn(), id, state, avg_color, None)
    }

    /// Build (or rebuild) one preview and record its average colour.
    pub fn make_thumbnail(&self, item: &Item) -> Result<()> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let item = self.checked_item(item)?;
        self.make_thumbnail_unlocked(&item)
    }

    fn make_thumbnail_unlocked(&self, item: &Item) -> Result<()> {
        if item.kind == Kind::Other {
            db::set_thumb_state(&self.conn(), &item.id, ThumbState::None, None, None)?;
            return Ok(());
        }
        let tools = ffmpeg::Tools::discover();
        let source = self.paths.abs(&item.rel_path);
        let outcome = thumb::generate(
            &self.paths,
            &item.id,
            &source,
            item.kind,
            item.orientation,
            item.duration_ms,
            &tools,
        );
        match outcome {
            Ok(outcome) => {
                db::set_thumb_state(
                    &self.conn(),
                    &item.id,
                    ThumbState::Ready,
                    Some(outcome.avg_color),
                    None,
                )?;
                Ok(())
            }
            Err(error) => {
                // Keep the reason: a preview that will not decode should say
                // why, rather than looking identical to one never attempted.
                let text: String = error.to_string().chars().take(400).collect();
                db::set_thumb_state(
                    &self.conn(),
                    &item.id,
                    ThumbState::Failed,
                    None,
                    Some(&text),
                )?;
                Err(error)
            }
        }
    }

    fn checked_item(&self, stale: &Item) -> Result<Item> {
        let current = db::get_item(&self.conn(), &stale.id)?
            .ok_or_else(|| Error::msg("item no longer exists"))?;
        if current.rel_path != stale.rel_path || current.checksum != stale.checksum {
            return Err(Error::msg(
                "item changed; reload it before opening its preview",
            ));
        }
        self.paths.checked_owned_path(&current.rel_path)?;
        for path in Paths::preview_rel_paths(&current.id) {
            self.paths.checked_owned_path(&path)?;
        }
        if let Some(vault) = db::item_vault_id(&self.conn(), &current.id)?
            && !self.is_unlocked(vault)
        {
            return Err(Error::msg("unlock this collection first"));
        }
        Ok(current)
    }

    // --- Semantic search -------------------------------------------------

    /// Stored vectors for `model`, in the order they were written. Rows with
    /// a different width are ignored; they belong to another index revision.
    pub fn embeddings(&self, model: &str, dims: usize) -> Result<Vec<(String, Vec<f32>)>> {
        Ok(db::embeddings(&self.conn(), model)?
            .into_iter()
            .filter(|(_, vector)| vector.len() == dims)
            .collect())
    }

    /// Stored vectors in the index's own layout: ids plus one contiguous
    /// buffer. One pass, one allocation, no per-vector copy.
    pub fn flat_embeddings(&self, model: &str, dims: usize) -> Result<(Vec<String>, Vec<f32>)> {
        db::flat_embeddings(&self.conn(), model, dims)
    }

    /// Newest media that has no vector for this model yet.
    pub fn pending_embeddings(&self, model: &str, limit: usize) -> Result<Vec<Item>> {
        db::pending_embeddings(&self.conn(), model, limit)
    }

    pub fn set_embedding(&self, id: &str, model: &str, vector: &[f32]) -> Result<()> {
        db::set_embeddings(&self.conn(), model, &[(id.to_string(), vector.to_vec())])
    }

    /// Commit several vectors in one transaction.
    pub fn set_embeddings(&self, model: &str, rows: &[(String, Vec<f32>)]) -> Result<()> {
        db::set_embeddings(&self.conn(), model, rows)
    }

    pub fn embedding_count(&self, model: &str) -> Result<usize> {
        db::embedding_count(&self.conn(), model)
    }

    /// The JPEG the vision encoder sees: the cached 384px preview, built on
    /// demand for libraries that have not generated one yet. Previews are
    /// already upright and flattened, and keep the upload small.
    pub fn embedding_frame(&self, item: &Item) -> Result<Vec<u8>> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let item = self.checked_item(item)?;
        let path = self.paths.thumb_path(&item.id);
        if item.thumb_recipe != thumb::RECIPE || !path.is_file() {
            self.make_thumbnail_unlocked(&item)?;
        }
        std::fs::read(&path)
            .map_err(|error| Error::Msg(format!("read {}: {error}", path.display())))
    }

    /// Decoded viewer preview (long edge capped), ready for the GPU.
    pub fn load_preview_rgba(&self, item: &Item, max_side: u32) -> Result<(u32, u32, Vec<u8>)> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let item = self.checked_item(item)?;
        // A proxy built by an older recipe is rebuilt before it is shown. The
        // old proxy still beats none, so a failed rebuild is not fatal here.
        if item.thumb_recipe != thumb::RECIPE {
            let _ = self.make_thumbnail_unlocked(&item);
        }
        let tools = if self.paths.viewer_jpeg_path(&item.id).is_file()
            || self.paths.viewer_png_path(&item.id).is_file()
        {
            None
        } else {
            Some(ffmpeg::Tools::discover())
        };
        thumb::preview_rgba(
            &self.paths,
            &item.id,
            &self.paths.abs(&item.rel_path),
            item.kind,
            item.orientation,
            item.duration_ms,
            max_side,
            tools.as_ref(),
        )
    }

    /// Full-resolution decode for close inspection, reduced to `max_side` at
    /// most. It always works from the original, never the cached proxy, so
    /// the viewer can show detail the 2048 px proxy does not have.
    pub fn load_full_rgba(&self, item: &Item, max_side: u32) -> Result<(u32, u32, Vec<u8>)> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let item = self.checked_item(item)?;
        let tools = ffmpeg::Tools::discover();
        thumb::full_rgba(
            &self.paths,
            &self.paths.abs(&item.rel_path),
            item.kind,
            item.orientation,
            item.duration_ms,
            max_side,
            Some(&tools),
        )
    }

    /// Absolute path of an item's stored original, for code that opens the
    /// file itself (video and animation players stream from it).
    pub fn source_path(&self, item: &Item) -> PathBuf {
        self.paths.abs(&item.rel_path)
    }

    /// Decoded 384px preview, ready to upload to the GPU. A preview built by
    /// an older recipe (or one that went missing) is rebuilt first, so a
    /// changed preview size or quality never leaves stale tiles behind.
    pub fn load_thumb_rgba(&self, item: &Item) -> Result<(u32, u32, Vec<u8>)> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let item = self.checked_item(item)?;
        let path = self.paths.thumb_path(&item.id);
        if item.thumb_recipe != thumb::RECIPE || !path.is_file() {
            self.make_thumbnail_unlocked(&item)?;
        }
        let image = crate::decode::open(&path)?.to_rgba8();
        let (w, h) = image.dimensions();
        Ok((w, h, image.into_raw()))
    }

    // --- Trash -----------------------------------------------------------

    /// Soft delete. The file stays in the store until the trash is emptied.
    pub fn trash(&self, ids: &[String]) -> Result<usize> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        db::trash(&self.conn(), ids, time::now_ms())
    }

    pub fn restore(&self, ids: &[String]) -> Result<usize> {
        let _files = self.files.read().unwrap_or_else(|e| e.into_inner());
        db::restore(&self.conn(), ids)
    }

    /// Permanently remove a catalogue row and its unreferenced files.
    /// Permanently delete items. Every path is checked before anything is
    /// removed, the rows go in one transaction, and only then are the files
    /// removed, each one only if no surviving row still points at it.
    pub fn delete_forever(&self, ids: &[String]) -> Result<usize> {
        let _files = write_gate(&self.files);
        let mut items = Vec::new();
        for id in ids {
            if let Some(item) = self.item(id)? {
                self.paths.checked_owned_path(&item.rel_path)?;
                items.push(item);
            }
        }
        let rows: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        db::delete_items(&self.conn(), &rows)?;
        for item in &items {
            self.remove_item_files(item)?;
        }
        Ok(items.len())
    }

    /// Permanently delete every item that has sat in the trash for longer
    /// than [`TRASH_RETENTION_DAYS`]. Restored items carry no timer, so
    /// trashing them again starts a fresh window.
    pub fn purge_expired_trash(&self) -> Result<usize> {
        self.purge_trash_before(time::now_ms() - TRASH_RETENTION_DAYS * DAY_MS)
    }

    fn purge_trash_before(&self, cutoff: i64) -> Result<usize> {
        let expired: Vec<String> = db::list_items(
            &self.conn(),
            &TimelineFilter {
                trashed: true,
                ..Default::default()
            },
        )?
        .into_iter()
        .filter(|item| item.deleted_at.is_some_and(|at| at <= cutoff))
        .map(|item| item.id)
        .collect();
        self.delete_forever(&expired)
    }

    pub fn empty_trash(&self) -> Result<usize> {
        let items = db::list_items(
            &self.conn(),
            &TimelineFilter {
                trashed: true,
                ..Default::default()
            },
        )?;
        let ids: Vec<String> = items.into_iter().map(|item| item.id).collect();
        self.delete_forever(&ids)
    }

    fn remove_item_files(&self, item: &Item) -> Result<()> {
        self.paths.checked_owned_path(&item.rel_path)?;
        store::remove_unreferenced(&self.paths, &self.conn(), &item.rel_path);
        let _ = std::fs::remove_file(self.paths.thumb_path(&item.id));
        let _ = std::fs::remove_file(self.paths.viewer_jpeg_path(&item.id));
        let _ = std::fs::remove_file(self.paths.viewer_png_path(&item.id));
        Ok(())
    }

    // --- Integrity -------------------------------------------------------

    pub fn verify(&self, options: VerifyOptions) -> Result<VerifyReport> {
        verify::run(self, options)
    }

    /// Write a complete, consistent copy of the library into `dest`.
    ///
    /// Imports, optimizations, deletion and preview writes are paused for the
    /// duration of the copy. The catalogue uses SQLite's online backup API.
    /// The result is published only after every file and the manifest succeeds.
    /// Do not call this from an import or optimization progress callback.
    pub fn backup(&self, dest: impl AsRef<Path>) -> Result<BackupReport> {
        let _files = self.files.write().unwrap_or_else(|e| e.into_inner());
        let dest = dest.as_ref();
        let root = self
            .paths
            .root()
            .canonicalize()
            .unwrap_or_else(|_| self.paths.root().to_path_buf());
        let dest_abs = if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            std::env::current_dir()?.join(dest)
        };
        std::fs::create_dir_all(&dest_abs)?;
        // Canonicalize after creating it: before that, a path that does not
        // exist yet cannot be compared with the library's canonical root.
        let dest_canonical = dest_abs.canonicalize()?;
        if dest_canonical.starts_with(&root) {
            return Err(Error::Msg(
                "choose a backup folder outside the library".into(),
            ));
        }
        let name = format!("ForbiddenLibrary-backup-{}", time::stamp());
        let staging = dest_canonical.join(format!(".{name}.partial"));
        let final_dir = dest_canonical.join(&name);
        if staging.exists() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        std::fs::create_dir_all(&staging)?;

        let write = || -> Result<BackupReport> {
            let mut report = BackupReport::default();
            // The file gate keeps the store stable through the snapshot.
            copy_tree(self.paths.store(), &staging.join("store"), &mut report)?;
            copy_tree(self.paths.thumbs(), &staging.join("thumbs"), &mut report)?;
            // Sealed collections are ciphertext, and a backup that left them out would
            // restore a library missing its locked items.
            copy_tree(self.paths.vault(), &staging.join("vault"), &mut report)?;
            let snapshot = staging.join("library.db");
            {
                // SQLite's online backup takes a consistent snapshot while
                // WAL is live, without copying uncheckpointed frames verbatim
                // and without blocking readers.
                let conn = self.conn();
                let mut copy = rusqlite::Connection::open(&snapshot)?;
                {
                    let backup = rusqlite::backup::Backup::new(&conn, &mut copy)?;
                    backup.run_to_completion(256, std::time::Duration::from_millis(0), None)?;
                }
                drop(copy);
                report.items = conn.query_row(
                    "SELECT COUNT(*) FROM items WHERE deleted_at IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )? as usize;
            }
            let manifest = serde_json::json!({
                "app": "Forbidden Library",
                "version": env!("CARGO_PKG_VERSION"),
                "schema": db::SCHEMA_VERSION,
                "created_at": time::now_ms(),
                "items": report.items,
                "files": report.files,
                "bytes": report.bytes,
            });
            std::fs::write(
                staging.join("backup.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            Ok(report)
        };

        match write() {
            Ok(mut report) => {
                std::fs::rename(&staging, &final_dir)?;
                // Report the path the user typed, not the canonical one with
                // Windows' `\\?\` prefix.
                report.path = dest_abs.join(&name).to_string_lossy().into_owned();
                Ok(report)
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(error)
            }
        }
    }
}

/// Copy every file under `from` into `to`, preserving the relative layout.
fn copy_tree(from: &Path, to: &Path, report: &mut BackupReport) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    let mut stack = vec![from.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if crate::paths::is_link(&std::fs::symlink_metadata(&path)?) {
                return Err(Error::msg("a backup source contains a reparse point"));
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(relative) = path.strip_prefix(from) else {
                continue;
            };
            let target = to.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &target)?;
            report.files += 1;
            report.bytes += std::fs::metadata(&target)
                .map(|meta| meta.len())
                .unwrap_or(0);
        }
    }
    Ok(())
}

/// Format bytes the way the UI shows them: 0 B, 999 KB, 1.4 MB, 23 GB.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        let text = format!("{value:.1}");
        let text = text.strip_suffix(".0").unwrap_or(&text);
        format!("{text} {}", UNITS[unit])
    }
}

/// Format an epoch-millisecond timestamp in local time using a short,
/// sentence-case date label.
pub fn format_date(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(dt) = Local.timestamp_millis_opt(ms).single() else {
        return "Unknown date".into();
    };
    dt.format("%-d %B %Y").to_string()
}

/// Sortable local day key (`2026-10-23`) used to group the timeline.
pub fn date_key(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "0000-00-00".into())
}

/// The local calendar day of a timestamp, as a number that rises by one each
/// day. Two timestamps share a number exactly when [`date_key`] gives them the
/// same text, so grouping can compare numbers instead of formatting strings.
pub fn local_day(ms: i64) -> Option<i64> {
    use chrono::{Datelike, Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| i64::from(dt.date_naive().num_days_from_ce()))
}

/// Full local date and time for the info panel.
pub fn format_datetime(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(dt) = Local.timestamp_millis_opt(ms).single() else {
        return "Unknown date".into();
    };
    dt.format("%-d %B %Y at %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_photo(path: &Path) {
        let image = image::RgbImage::from_fn(48, 32, |x, y| {
            image::Rgb([(x * 5) as u8, (y * 7) as u8, 60])
        });
        image.save(path).unwrap();
    }

    #[test]
    fn backup_waits_for_file_mutations_and_includes_their_commits() {
        let temp = tempfile::tempdir().unwrap();
        let library = Library::open(temp.path().join("library")).unwrap();
        let source = temp.path().join("photo.png");
        sample_photo(&source);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (backup_tx, backup_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let library_ref = &library;
            scope.spawn(move || {
                import_paths(
                    library_ref,
                    &[source],
                    &ImportOptions::default(),
                    &mut |event| {
                        if matches!(event, ImportEvent::Started { .. }) {
                            started_tx.send(()).unwrap();
                            release_rx.recv().unwrap();
                        }
                    },
                );
            });
            started_rx.recv().unwrap();
            scope.spawn(|| {
                backup_tx
                    .send(library.backup(temp.path().join("backups")))
                    .unwrap();
            });
            let early_backup = backup_rx.recv_timeout(std::time::Duration::from_millis(100));
            release_tx.send(()).unwrap();
            assert!(early_backup.is_err(), "backup ran during an active import");
        });
        let report = backup_rx.recv().unwrap().unwrap();
        let restored = Library::open(report.path).unwrap();
        assert_eq!(restored.item_count(&TimelineFilter::default()).unwrap(), 1);
        let verified = restored.verify(VerifyOptions::default()).unwrap();
        assert!(verified.missing.is_empty());
        assert!(verified.corrupt.is_empty());
    }

    #[test]
    fn the_trash_countdown_counts_whole_days() {
        assert_eq!(TRASH_RETENTION_DAYS, 30);
        assert_eq!(trash_days_left(0, 0), 30);
        assert_eq!(trash_days_left(0, DAY_MS), 29);
        assert_eq!(trash_days_left(0, 30 * DAY_MS - 1), 1);
        assert_eq!(trash_days_left(0, 30 * DAY_MS), 0);
        assert_eq!(trash_days_left(0, 99 * DAY_MS), 0);
    }

    #[test]
    fn expired_trash_is_deleted_and_restoring_resets_the_clock() {
        let root = std::env::temp_dir().join(format!(
            "forbiddenlibrary-retention-{}-{}",
            std::process::id(),
            time::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("photo.png");
        sample_photo(&source);
        let library = Library::open(root.join("library")).unwrap();
        import_paths(
            &library,
            std::slice::from_ref(&source),
            &ImportOptions::default(),
            &mut |_| {},
        );
        let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
        let stored = library.paths().abs(&item.rel_path);
        let id = item.id.clone();

        // Trashed 29 days ago: still inside the window.
        library.trash(std::slice::from_ref(&id)).unwrap();
        db::restore(&library.conn(), std::slice::from_ref(&id)).unwrap();
        db::trash(
            &library.conn(),
            std::slice::from_ref(&id),
            time::now_ms() - 29 * DAY_MS,
        )
        .unwrap();
        assert_eq!(library.purge_expired_trash().unwrap(), 0);
        assert!(library.item(&id).unwrap().is_some());

        // Restoring and trashing again starts a fresh window.
        library.restore(std::slice::from_ref(&id)).unwrap();
        library.trash(std::slice::from_ref(&id)).unwrap();
        let fresh = library.item(&id).unwrap().unwrap();
        assert_eq!(
            trash_days_left(fresh.deleted_at.unwrap(), time::now_ms()),
            TRASH_RETENTION_DAYS
        );

        // Trashed 31 days ago: gone on the next sweep, files included.
        db::restore(&library.conn(), std::slice::from_ref(&id)).unwrap();
        db::trash(
            &library.conn(),
            std::slice::from_ref(&id),
            time::now_ms() - 31 * DAY_MS,
        )
        .unwrap();
        assert_eq!(library.purge_expired_trash().unwrap(), 1);
        assert!(library.item(&id).unwrap().is_none());
        assert!(!stored.exists());

        drop(library);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn local_days_group_exactly_as_date_keys_do() {
        let hour = 3_600_000i64;
        let stamps = [
            0,
            hour,
            23 * hour,
            24 * hour,
            25 * hour,
            3 * 24 * hour + 5,
            -hour * 40,
        ];
        for a in stamps {
            for b in stamps {
                assert_eq!(
                    date_key(a) == date_key(b),
                    local_day(a) == local_day(b),
                    "{a} and {b} group differently"
                );
            }
        }
    }
}
