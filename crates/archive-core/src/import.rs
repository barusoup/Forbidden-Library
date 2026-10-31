use crate::Library;
use crate::audio::{self, Attempt, Meta as AudioMeta};
use crate::db;
use crate::error::{Error, Result};
use crate::ffmpeg;
use crate::lyrics;
use crate::model::{ImportReport, Item, Kind, OptState, ThumbState};
use crate::optimize::{self, OptimizeConfig};
use crate::probe;
use crate::store;
use crate::thumb;
use crate::time;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    /// Keep non-image/non-video files as opaque archive entries.
    pub include_other: bool,
    /// Once set, no further files are started. Files already being copied
    /// finish and are committed, so the report lists everything that landed.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Rewrite images that verifiably shrink before they reach the store. `None`
    /// stores every file as it was given.
    pub optimize: Option<OptimizeConfig>,
}

impl ImportOptions {
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }
}

#[derive(Clone, Debug)]
pub enum ImportEvent {
    Scanning,
    Started {
        total: usize,
        total_bytes: Option<u64>,
    },
    FileStarted {
        index: usize,
        total: usize,
        path: PathBuf,
    },
    Copying {
        copied: u64,
        bytes: u64,
    },
    FileDone {
        item: Box<Item>,
        duplicate: bool,
    },
    FileFailed {
        path: PathBuf,
        error: String,
    },
    Finished(ImportReport),
}

/// Everything a worker needs to report back. Only the thread that owns the
/// progress callback touches `ImportReport`, so workers never share it.
enum Message {
    Started { index: usize, path: PathBuf },
    Copying { copied: u64, bytes: u64 },
    Done(Box<Done>),
}

struct Done {
    path: PathBuf,
    outcome: Result<Outcome>,
}

/// A file that is stored and catalogued-in-memory but still needs its
/// preview rendered before the row can be committed.
struct Prepared {
    item: Item,
    path: PathBuf,
    warning: Option<String>,
    audio: Option<AudioMeta>,
    lyrics: Option<String>,
}

enum Outcome {
    Imported {
        item: Item,
        warning: Option<String>,
        audio: Option<Box<AudioMeta>>,
        lyrics: Option<String>,
    },
    Duplicate {
        item: Option<Item>,
        bytes: u64,
    },
    Skipped {
        warning: Option<String>,
    },
}

/// Store files that imports have installed but not yet committed, counted by
/// their catalogue-relative path. A file is only referenced once its row
/// commits, so until then a preview failure or a lost race must not delete it:
/// another import may be holding the same bytes.
///
/// Install and release take the same lock, so a file cannot be deleted between
/// another worker's install and that worker's hold.
/// One table per library, not per import: two imports can run at once (a
/// dropped folder and a phone transfer), and either may install the bytes the
/// other is still holding.
#[derive(Default)]
pub(crate) struct InFlight(Mutex<HashMap<String, usize>>);

impl InFlight {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Install a staged file and hold it until the caller releases it.
    fn install(&self, library: &Library, staged: &store::Staged, ext: &str) -> Result<String> {
        let mut holds = self.lock();
        let rel = store::install(library.paths(), staged, ext)?;
        *holds.entry(rel.clone()).or_default() += 1;
        Ok(rel)
    }

    /// Give back one hold. The file is deleted only when no import holds it
    /// and no catalogue row refers to it, which also cleans up a file a lost
    /// race left unreferenced.
    fn release(&self, library: &Library, rel: &str) {
        let mut holds = self.lock();
        if let Some(count) = holds.get_mut(rel) {
            *count -= 1;
            if *count == 0 {
                holds.remove(rel);
            }
        }
        if !holds.contains_key(rel) {
            store::remove_unreferenced(library.paths(), &library.conn(), rel);
        }
    }
}

/// Rows per catalogue commit. `synchronous = FULL` makes every commit flush
/// the WAL; one flush for a group of files instead of one per file is the
/// difference between a folder of small files importing slowly and quickly.
/// Sixty-four rows still reach the interface in well under a blink.
const IMPORT_BATCH: usize = 64;

/// Commit a partial batch once its oldest row has waited this long, so a
/// single slow file at the end of a folder does not hold back the files that
/// already finished.
const BATCH_DELAY: Duration = Duration::from_millis(150);

/// An imported file waiting for its catalogue row to be committed with its
/// neighbours.
struct Pending {
    item: Item,
    path: PathBuf,
    warning: Option<String>,
    audio: Option<AudioMeta>,
    lyrics: Option<String>,
}

/// Import files and folders. Everything is copied: the source is never
/// moved, renamed or modified. Each file is staged and fsynced before it
/// enters the content-addressed store, and the catalogue rows are committed
/// only after the stored files exist.
///
/// Files are imported by a small pool of workers. Probing, copying, decoding
/// a preview and hashing are mostly independent per file, so a folder of
/// photos imports several times faster than a serial loop on the same disk.
pub fn import_paths(
    library: &Library,
    sources: &[PathBuf],
    options: &ImportOptions,
    on_event: &mut dyn FnMut(ImportEvent),
) -> ImportReport {
    let _files = library.files.read().unwrap_or_else(|e| e.into_inner());
    let mut report = ImportReport::default();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    on_event(ImportEvent::Scanning);
    // The library root is canonicalized once, not once per file found.
    let canonical_root = library
        .root()
        .canonicalize()
        .unwrap_or_else(|_| library.root().to_path_buf());
    for source in sources {
        collect(source, &canonical_root, &mut files, &mut seen, &mut report);
    }
    // An `.lrc` is only kept beside the song it describes. Any other is dropped
    // here, so it is never copied as an opaque file of its own.
    let pairing = lyrics::pair(files);
    report.skipped += pairing.discarded;
    let sidecars = pairing.sidecars;
    let mut files = pairing.files;
    files.sort();
    let total = files.len();
    on_event(ImportEvent::Started {
        total,
        total_bytes: None,
    });
    if total == 0 {
        on_event(ImportEvent::Finished(report.clone()));
        return report;
    }

    let tools = ffmpeg::Tools::discover();
    let in_flight = &library.in_flight;
    let io_threads = import_threads(total);
    let preview_pool = preview_threads(total, io_threads);
    let next = AtomicUsize::new(0);
    let (finished_tx, finished_rx) = mpsc::channel::<Message>();
    // Back-pressure: the copy stage may run ahead of the preview pool by a
    // couple of files per worker, and no further. Everything past that is
    // already stored, so the bound only limits memory.
    let prepared: WorkQueue<Prepared> = WorkQueue::new(io_threads + preview_pool * 2);
    std::thread::scope(|scope| {
        // Stage one: dedupe, probe, copy, hash and move into the store.
        let mut io = Vec::with_capacity(io_threads);
        for _ in 0..io_threads {
            let finished = finished_tx.clone();
            let files = &files;
            let sidecars = &sidecars;
            let next = &next;
            let tools = &tools;
            let prepared = &prepared;
            io.push(scope.spawn(move || {
                loop {
                    if options.cancelled() {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(index) else { break };
                    if finished
                        .send(Message::Started {
                            index,
                            path: path.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    let mut progress = |copied: u64, bytes: u64| {
                        let _ = finished.send(Message::Copying { copied, bytes });
                    };
                    let sidecar = sidecars.get(path).map(PathBuf::as_path);
                    match import_one(
                        library,
                        tools,
                        path,
                        options,
                        sidecar,
                        in_flight,
                        &mut progress,
                    ) {
                        Ok(Outcome::Imported {
                            item,
                            warning,
                            audio,
                            lyrics,
                        }) => {
                            let job = Prepared {
                                item,
                                path: path.clone(),
                                warning,
                                audio: audio.map(|meta| *meta),
                                lyrics,
                            };
                            if !prepared.push(job) {
                                break;
                            }
                        }
                        outcome => {
                            let done = Done {
                                path: path.clone(),
                                outcome,
                            };
                            if finished.send(Message::Done(Box::new(done))).is_err() {
                                break;
                            }
                        }
                    }
                }
            }));
        }

        // Stage two: render previews. A wide pool is what lets a folder of
        // photos import at decode speed: a copy worker is never held up by a
        // twenty megapixel decode it could hand to someone else.
        for _ in 0..preview_pool {
            let finished = finished_tx.clone();
            let prepared = &prepared;
            let tools = &tools;
            scope.spawn(move || {
                while let Some(job) = prepared.pop() {
                    let Prepared {
                        mut item,
                        path,
                        warning,
                        mut audio,
                        lyrics,
                    } = job;
                    let mut refusal: Option<String> = None;
                    if item.kind == Kind::Audio {
                        // The cover is the preview. A file without one is still
                        // music, so it is kept and its tile is drawn without art.
                        let source = library.paths().abs(&item.rel_path);
                        match thumb::generate(
                            library.paths(),
                            &item.id,
                            &source,
                            item.kind,
                            item.orientation,
                            item.duration_ms,
                            tools,
                        ) {
                            Ok(outcome) => {
                                item.thumb_state = ThumbState::Ready;
                                item.avg_color = outcome.avg_color;
                                if let Some(meta) = audio.as_mut() {
                                    meta.has_cover = true;
                                }
                            }
                            Err(_) => item.thumb_state = ThumbState::None,
                        }
                    } else if matches!(item.kind, Kind::Image | Kind::Video) {
                        let source = library.paths().abs(&item.rel_path);
                        let generated = if forced_preview_failure(&path) {
                            Err(Error::msg("the preview was forced to fail"))
                        } else {
                            thumb::generate(
                                library.paths(),
                                &item.id,
                                &source,
                                item.kind,
                                item.orientation,
                                item.duration_ms,
                                tools,
                            )
                        };
                        match generated {
                            Ok(outcome) => {
                                item.thumb_state = ThumbState::Ready;
                                item.avg_color = outcome.avg_color;
                            }
                            // Media that cannot be decoded is refused: no row,
                            // and the copy is released below. The reason is kept
                            // so the import report can say why.
                            Err(error) => refusal = Some(error.to_string()),
                        }
                    }
                    let outcome = if let Some(reason) = refusal {
                        in_flight.release(library, &item.rel_path);
                        Outcome::Skipped {
                            warning: Some(format!(
                                "could not be decoded, so it was not imported: {reason}"
                            )),
                        }
                    } else {
                        Outcome::Imported {
                            item,
                            warning,
                            audio: audio.map(Box::new),
                            lyrics,
                        }
                    };
                    let done = Done {
                        path,
                        outcome: Ok(outcome),
                    };
                    if finished.send(Message::Done(Box::new(done))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(finished_tx);

        let mut pending: Vec<Pending> = Vec::new();
        let mut batch_started = Instant::now();
        let mut closed = false;
        loop {
            // The copy stage is the only producer, so once it is done the
            // preview pool is told to finish.
            if !closed && io.iter().all(|handle| handle.is_finished()) {
                prepared.close();
                closed = true;
            }
            let message = match finished_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !pending.is_empty() && batch_started.elapsed() >= BATCH_DELAY {
                        commit_pending(library, in_flight, &mut pending, &mut report, on_event);
                        batch_started = Instant::now();
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match message {
                Message::Started { index, path } => {
                    on_event(ImportEvent::FileStarted { index, total, path });
                }
                Message::Copying { copied, bytes } => {
                    on_event(ImportEvent::Copying { copied, bytes });
                }
                Message::Done(done) => {
                    let Done { path, outcome } = *done;
                    match outcome {
                        Ok(Outcome::Imported {
                            item,
                            warning,
                            audio,
                            lyrics,
                        }) => {
                            pending.push(Pending {
                                item,
                                path,
                                warning,
                                audio: audio.map(|meta| *meta),
                                lyrics,
                            });
                        }
                        Ok(Outcome::Duplicate { item, bytes }) => {
                            report.duplicates += 1;
                            report.bytes_deduplicated += bytes;
                            if let Some(item) = item {
                                on_event(ImportEvent::FileDone {
                                    item: Box::new(item),
                                    duplicate: true,
                                });
                            }
                        }
                        Ok(Outcome::Skipped { warning }) => {
                            report.skipped += 1;
                            if let Some(warning) = warning {
                                report.failures.push((path.display().to_string(), warning));
                            }
                        }
                        Err(error) => {
                            let message = error.to_string();
                            report.failed += 1;
                            report
                                .failures
                                .push((path.display().to_string(), message.clone()));
                            {
                                let conn = library.conn();
                                let _ = db::record_failure(&conn, &path, &message);
                            }
                            on_event(ImportEvent::FileFailed {
                                path,
                                error: message,
                            });
                        }
                    }
                }
            }
            if pending.len() >= IMPORT_BATCH {
                commit_pending(library, in_flight, &mut pending, &mut report, on_event);
                batch_started = Instant::now();
            }
        }
        commit_pending(library, in_flight, &mut pending, &mut report, on_event);
    });
    on_event(ImportEvent::Finished(report.clone()));
    report
}

/// A bounded hand-off queue.
///
/// `std::sync::mpsc` has one consumer and the preview pool has several. A
/// mutex and two condition variables are enough: a push blocks while the
/// queue is full (back-pressure), a pop blocks while it is empty, and closing
/// wakes everyone so the workers can leave once the producers are done.
struct WorkQueue<T> {
    state: std::sync::Mutex<QueueState<T>>,
    ready: std::sync::Condvar,
    space: std::sync::Condvar,
    capacity: usize,
}

struct QueueState<T> {
    items: std::collections::VecDeque<T>,
    closed: bool,
}

impl<T> WorkQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(QueueState {
                items: std::collections::VecDeque::new(),
                closed: false,
            }),
            ready: std::sync::Condvar::new(),
            space: std::sync::Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState<T>> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Wait for room, then queue `item`. False means the queue closed first.
    fn push(&self, item: T) -> bool {
        let mut state = self.lock();
        while state.items.len() >= self.capacity && !state.closed {
            state = self
                .space
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        if state.closed {
            return false;
        }
        state.items.push_back(item);
        self.ready.notify_one();
        true
    }

    fn pop(&self) -> Option<T> {
        let mut state = self.lock();
        loop {
            if let Some(item) = state.items.pop_front() {
                self.space.notify_one();
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.ready.notify_all();
        self.space.notify_all();
    }
}

/// Write a group of imported files to the catalogue in one transaction and
/// report each of them once the rows are durable.
///
/// A row that loses the race against an identical file (in this batch or in a
/// concurrent import) is a duplicate, not a failure: the bytes are stored once
/// and both sources are recorded.
fn commit_pending(
    library: &Library,
    in_flight: &InFlight,
    pending: &mut Vec<Pending>,
    report: &mut ImportReport,
    on_event: &mut dyn FnMut(ImportEvent),
) {
    if pending.is_empty() {
        return;
    }
    let rows: Vec<(&Item, &Path, Option<&AudioMeta>)> = pending
        .iter()
        .map(|entry| (&entry.item, entry.path.as_path(), entry.audio.as_ref()))
        .collect();
    let inserted = {
        let conn = library.conn();
        db::insert_items(&conn, &rows)
    };
    // `Some(true)` inserted, `Some(false)` already catalogued, `None` failed
    // (and was reported) while retrying the batch row by row.
    let flags: Vec<Option<bool>> = match inserted {
        Ok(flags) => flags.into_iter().map(Some).collect(),
        Err(_) => {
            // One unacceptable row must not cost the batch. Retrying row by
            // row keeps the failure local to the file that caused it.
            let mut flags = Vec::with_capacity(pending.len());
            for entry in pending.iter() {
                let inserted = {
                    let conn = library.conn();
                    db::insert_item(&conn, &entry.item, &entry.path, entry.audio.as_ref())
                };
                match inserted {
                    Ok(inserted) => flags.push(Some(inserted)),
                    Err(error) => {
                        let message = error.to_string();
                        report.failed += 1;
                        report
                            .failures
                            .push((entry.path.display().to_string(), message.clone()));
                        {
                            let conn = library.conn();
                            let _ = db::record_failure(&conn, &entry.path, &message);
                        }
                        on_event(ImportEvent::FileFailed {
                            path: entry.path.clone(),
                            error: message,
                        });
                        flags.push(None);
                    }
                }
            }
            flags
        }
    };
    for (entry, inserted) in pending.iter().zip(flags) {
        let Some(inserted) = inserted else {
            continue;
        };
        if let Some(lrc) = &entry.lyrics {
            // Lyrics are attached after the row exists. A song that was already
            // catalogued keeps the lyrics it has.
            let conn = library.conn();
            let _ = db::set_lyrics(&conn, &entry.item.id, lrc);
        }
        if inserted {
            report.imported += 1;
            report.bytes_imported += entry.item.bytes;
            report.created.push(entry.item.id.clone());
            if let Some(warning) = &entry.warning {
                report
                    .failures
                    .push((entry.path.display().to_string(), warning.clone()));
            }
            on_event(ImportEvent::FileDone {
                item: Box::new(entry.item.clone()),
                duplicate: false,
            });
        } else {
            report.duplicates += 1;
            report.bytes_deduplicated += entry.item.bytes;
            let item = library
                .item(&entry.item.id)
                .ok()
                .flatten()
                .unwrap_or_else(|| entry.item.clone());
            on_event(ImportEvent::FileDone {
                item: Box::new(item),
                duplicate: true,
            });
        }
    }
    // The rows are final now, so each hold ends. A file that lost its race
    // is released with no row pointing at it, and is removed here.
    for entry in pending.iter() {
        in_flight.release(library, &entry.item.rel_path);
    }
    pending.clear();
}

/// Copy workers. Importing is I/O bound once previews run elsewhere, and a
/// pool larger than the disk's queue depth only adds seek churn.
fn import_threads(total: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4);
    (cores / 2).clamp(2, 6).min(total).max(1)
}

/// Preview workers.
///
/// Decoding a 12 megapixel photo costs several times what copying it does, so
/// a preview pool no wider than the copy pool would cap a photo import at one
/// decode per copy worker. The extra workers are what let the folder import
/// at decode speed. Each decode stays bounded by `decode::limits`, which keeps
/// one absurd image from eating the machine.
fn preview_threads(total: usize, io_threads: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4);
    (cores.saturating_sub(io_threads))
        .clamp(2, 8)
        .min(total)
        .max(1)
}

/// Test hook: a source whose file name is listed here fails its preview as if
/// the decoder had refused it. A transient failure cannot be made to happen on
/// demand any other way.
#[cfg(test)]
static FORCED_PREVIEW_FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn forced_preview_failure(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    FORCED_PREVIEW_FAILURES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .contains(&name)
}

#[cfg(not(test))]
fn forced_preview_failure(_path: &Path) -> bool {
    false
}

/// Attach a sidecar's lyrics to a song that is already catalogued, so that
/// re-importing a folder with new `.lrc` files adds the lyrics it lacked.
fn attach_existing_lyrics(library: &Library, item: Option<&Item>, sidecar: Option<&Path>) {
    let Some(item) = item else { return };
    if item.kind != Kind::Audio {
        return;
    }
    let Some(lrc) = sidecar.and_then(lyrics::read) else {
        return;
    };
    let conn = library.conn();
    let _ = db::set_lyrics(&conn, &item.id, &lrc);
}

fn import_one(
    library: &Library,
    tools: &ffmpeg::Tools,
    path: &Path,
    options: &ImportOptions,
    sidecar: Option<&Path>,
    in_flight: &InFlight,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(Outcome::Skipped { warning: None });
    }

    // A re-import is common: the same folder dropped twice, a phone dump
    // copied again. If no stored item has this byte length the file cannot be
    // a duplicate, so the source is normally not read here at all. When a
    // size match exists, hashing it is still far cheaper than staging a copy
    // and fsyncing it only to discover the same bytes are already stored.
    let bytes = metadata.len();
    let size_match = db::has_items_of_size(&library.conn(), bytes)?;
    let duplicate = if size_match {
        // Disk reads must not hold the catalogue mutex: a large source can
        // take seconds to hash while the UI still needs catalogue access.
        let hash = store::hash_file(path)?;
        db::find_id_by_content_hash(&library.conn(), &hash)?
    } else {
        None
    };
    if let Some(id) = duplicate {
        {
            let conn = library.conn();
            let _ = db::add_source(&conn, &id, path);
        }
        let item = library.item(&id).ok().flatten();
        attach_existing_lyrics(library, item.as_ref(), sidecar);
        return Ok(Outcome::Duplicate { item, bytes });
    }

    // Inspect before copying so a folder full of documents does not get
    // copied only to be thrown away.
    let info = match probe::inspect(path, tools) {
        Ok(info) => info,
        Err(error) => {
            return Ok(Outcome::Skipped {
                warning: Some(error.to_string()),
            });
        }
    };
    if info.kind == Kind::Other && !options.include_other {
        return Ok(Outcome::Skipped { warning: None });
    }
    // Only songs take lyrics. A sidecar beside anything else is not read.
    let lyrics = if info.kind == Kind::Audio {
        sidecar.and_then(lyrics::read)
    } else {
        None
    };

    let mut last_report = 0u64;
    let mut staged = store::stage_copy(path, library.paths().tmp(), |copied| {
        if copied - last_report >= 4 * 1024 * 1024 {
            last_report = copied;
            progress(copied, bytes);
        }
    })?;

    // Dedupe on both identity (the hash of the first import) and the
    // current content hash: re-importing an optimized file must also be a
    // duplicate, not a second archived copy.
    let existing_id = {
        let conn = library.conn();
        db::find_id_by_content_hash(&conn, &staged.hash)?
    };
    if let Some(id) = existing_id {
        staged.discard();
        {
            let conn = library.conn();
            let _ = db::add_source(&conn, &id, path);
        }
        let item = library.item(&id).ok().flatten();
        attach_existing_lyrics(library, item.as_ref(), sidecar);
        return Ok(Outcome::Duplicate {
            item,
            bytes: staged.bytes,
        });
    }

    // An image is rewritten while it is still staged, so the store only ever
    // holds the final bytes. The item keeps the hash of the original as its
    // identity, which keeps a re-import of the same file a duplicate.
    let original_hash = staged.hash.clone();
    let original_bytes = staged.bytes;
    let mut ext = info.ext.clone();
    let mut mime = info.mime.clone();
    let mut opt_state = if info.kind == Kind::Image {
        OptState::None
    } else {
        OptState::Skipped
    };
    let mut opt_kind = None;
    let mut opt_error = None;
    let source_ext = info.ext.to_ascii_lowercase();
    if let Some(config) = &options.optimize
        && optimize::is_candidate(info.kind, &source_ext, staged.bytes)
    {
        match optimize::best_replacement(
            &staged.tmp,
            &source_ext,
            library.paths().tmp(),
            tools,
            config,
        ) {
            Ok(Some(replacement)) => match store::replace_staged(&staged, &replacement.path) {
                Ok(next) => {
                    staged = next;
                    ext = replacement.ext;
                    mime = replacement.mime;
                    opt_state = OptState::Optimized;
                    opt_kind = Some(replacement.kind.to_string());
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&replacement.path);
                    opt_state = OptState::Failed;
                    opt_error = Some(error.to_string());
                }
            },
            // Nothing verified as smaller: the original is stored as it was.
            Ok(None) => {}
            Err(error) => {
                opt_state = OptState::Failed;
                opt_error = Some(error.to_string());
            }
        }
    }

    // Music is re-encoded on the same terms as images: only with the optimizer
    // on, and only when the result verifiably keeps the sound and its tags.
    // The item still carries the original's hash as its identity, as above.
    let mut audio = info.audio.clone();
    if let (Some(_), Some(meta)) = (&options.optimize, audio.as_mut()) {
        match audio::optimize(tools, &staged.tmp, meta, library.paths().tmp()) {
            Attempt::Keep => {}
            Attempt::Refused(reason) => {
                opt_state = OptState::Failed;
                opt_error = Some(reason);
            }
            Attempt::Replaced(candidate) => match store::replace_staged(&staged, &candidate.path) {
                Ok(next) => {
                    staged = next;
                    ext = candidate.ext.to_string();
                    mime = candidate.mime.to_string();
                    opt_state = OptState::Optimized;
                    opt_kind = Some(candidate.opt_kind.to_string());
                    *meta = candidate.meta;
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&candidate.path);
                    opt_state = OptState::Failed;
                    opt_error = Some(error.to_string());
                }
            },
        }
    }

    let rel_path = in_flight.install(library, &staged, &ext)?;
    let now = time::now_ms();
    let original_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".into());

    // The preview is rendered by the preview pool, which is what lets this
    // worker start copying the next file instead of decoding this one. The
    // row is committed only after the preview pool hands the item back, and
    // only `Ready` and `Failed` ever reach the catalogue.
    let thumb_state = if matches!(info.kind, Kind::Image | Kind::Video | Kind::Audio) {
        ThumbState::Pending
    } else {
        ThumbState::None
    };

    let item = Item {
        id: original_hash,
        kind: info.kind,
        mime,
        ext,
        original_ext: path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_else(|| info.ext.clone()),
        bytes: staged.bytes,
        original_bytes,
        width: info.width,
        height: info.height,
        duration_ms: info.duration_ms,
        captured_at: info.captured_at,
        captured_source: info.captured_source,
        added_at: now,
        modified_at: now,
        original_name,
        rel_path,
        thumb_state,
        opt_state,
        opt_kind,
        deleted_at: None,
        camera_make: info.camera_make,
        camera_model: info.camera_model,
        lens: info.lens,
        iso: info.iso,
        f_number: info.f_number,
        exposure: info.exposure,
        focal_mm: info.focal_mm,
        orientation: info.orientation,
        gps: info.gps,
        avg_color: 0x2b2b2b,
        checksum: staged.hash.clone(),
        content_hash: staged.hash.clone(),
        thumb_recipe: crate::thumb::RECIPE,
        thumb_error: None,
        opt_error,
    };

    // The row is committed by the collector in a batch with its neighbours:
    // one WAL flush for many files instead of one per file.
    Ok(Outcome::Imported {
        item,
        warning: None,
        audio: audio.map(Box::new),
        lyrics,
    })
}

fn collect(
    source: &Path,
    canonical_root: &Path,
    files: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    report: &mut ImportReport,
) {
    if source.is_file() {
        if let Ok(path) = source.canonicalize() {
            admit(path, canonical_root, files, seen);
        }
        return;
    }
    if !source.is_dir() {
        report.skipped += 1;
        return;
    }
    // Canonicalized once here. Links are not followed, so every path the walk
    // yields below is already canonical and needs no canonicalize of its own.
    let Ok(canonical_source) = source.canonicalize() else {
        report.skipped += 1;
        return;
    };
    for entry in WalkDir::new(&canonical_source)
        .follow_links(false)
        .into_iter()
    {
        let Ok(entry) = entry else {
            report.skipped += 1;
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name.eq_ignore_ascii_case("thumbs.db")
            || name.eq_ignore_ascii_case("desktop.ini")
            || name.starts_with("~$")
            || name.starts_with('.')
        {
            continue;
        }
        admit(entry.into_path(), canonical_root, files, seen);
    }
}

/// Keep a canonical file path unless it lives inside the library or was seen
/// already through another source.
fn admit(
    path: PathBuf,
    canonical_root: &Path,
    files: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) {
    if path.starts_with(canonical_root) {
        return;
    }
    if seen.insert(path.clone()) {
        files.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TimelineFilter;
    use image::{Rgb, RgbImage};

    fn png_bytes() -> Vec<u8> {
        let image = RgbImage::from_fn(64, 64, |x, y| Rgb([x as u8, y as u8, 7]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn a_shared_store_file_survives_one_copys_failed_preview() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        // Two copies of the same bytes share one store file. A failed preview
        // for the first copy must not delete the file the second one commits.
        std::fs::write(source.join("forced-copy.png"), png_bytes()).unwrap();
        std::fs::write(source.join("kept-copy.png"), png_bytes()).unwrap();
        FORCED_PREVIEW_FAILURES
            .lock()
            .unwrap()
            .push("forced-copy.png".into());

        let library = Library::open(temp.path().join("library")).unwrap();
        let report = import_paths(&library, &[source], &ImportOptions::default(), &mut |_| {});
        FORCED_PREVIEW_FAILURES
            .lock()
            .unwrap()
            .retain(|name| name != "forced-copy.png");

        assert_eq!(report.imported, 1, "{report:?}");
        let item = library.items(&TimelineFilter::default()).unwrap().remove(0);
        assert!(
            library.paths().abs(&item.rel_path).is_file(),
            "the committed item lost its file"
        );
    }

    #[test]
    fn a_held_file_is_removed_only_when_nothing_refers_to_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let library = Library::open(temp.path().join("library")).unwrap();
        let source = temp.path().join("loose.png");
        std::fs::write(&source, png_bytes()).unwrap();
        let staged = store::stage_copy(&source, library.paths().tmp(), |_| {}).unwrap();

        let in_flight = InFlight::default();
        let rel = in_flight.install(&library, &staged, "png").unwrap();
        assert!(library.paths().abs(&rel).is_file());
        // Nothing commits this file, so the last hold ending removes it.
        in_flight.release(&library, &rel);
        assert!(!library.paths().abs(&rel).exists());
    }
}
