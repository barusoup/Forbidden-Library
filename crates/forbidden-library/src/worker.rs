//! Background work. The UI thread never imports, decodes, optimizes or
//! hashes; it drains events and uploads finished pixels.

use archive_core::{
    ImportEvent, ImportOptions, ImportReport, Item, Library, OptimizeConfig, import_paths,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use winit::event_loop::EventLoopProxy;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TexKind {
    Thumb,
    Preview,
    /// The full-resolution still behind the viewer. Decoded on demand, and
    /// never cached in the preview LRU.
    Full,
}

impl TexKind {
    pub fn key(self, id: &str) -> String {
        match self {
            TexKind::Thumb => format!("t:{id}"),
            TexKind::Preview => format!("p:{id}"),
            TexKind::Full => format!("f:{id}"),
        }
    }
}

pub enum AppEvent {
    RemoteImportChanged,
    Import(ImportEvent),
    ImportDone(ImportReport),
    /// An import taken back: the rows are gone, or the removal failed.
    ImportDiscarded(Result<(), String>),
    Texture {
        key: String,
        id: String,
        kind: TexKind,
        /// The vault epoch the job was asked under. See [`TextureJob::epoch`].
        epoch: u64,
        result: Result<(u32, u32, Vec<u8>), String>,
    },
    ThumbRebuilt {
        id: String,
        result: Result<(), String>,
    },
    Paths(Vec<PathBuf>),
    /// Ranked item ids for a typed query, best first.
    SemanticResults {
        seq: u64,
        ids: Vec<String>,
    },
    /// New vectors landed; an active search may have new results.
    SemanticAdvanced,
    /// A lock, unlock or seal finished on its background thread.
    Vault(VaultDone),
}

/// What a vault job was, so its result is read the way it was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultKind {
    Create,
    Unlock,
    RemoveLock,
    Lock,
    LockAll,
}

pub struct VaultDone {
    pub collection: i64,
    pub kind: VaultKind,
    pub result: Result<(), archive_core::Error>,
}

pub struct TextureJob {
    pub key: String,
    pub kind: TexKind,
    pub item: Item,
    /// Long edge in physical pixels for a preview; ignored for thumbnails.
    pub max_side: u32,
    /// The vault epoch when the job was asked for. A collection locked after
    /// that makes the result stale, and the UI drops it rather than show it.
    pub epoch: u64,
}

/// The jobs waiting for a worker. `closed` is set once the pool is gone, and
/// the workers stop taking work.
struct Queue {
    jobs: VecDeque<TextureJob>,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The next job, waiting while there is none. `None` once the pool is gone.
    fn next(&self) -> Option<TextureJob> {
        let mut queue = self.lock();
        loop {
            if queue.closed {
                return None;
            }
            if let Some(job) = queue.jobs.pop_front() {
                return Some(job);
            }
            queue = self
                .ready
                .wait(queue)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

/// Marks the queue closed when the last handle to the pool is dropped.
struct Closer(Arc<Shared>);

impl Drop for Closer {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.ready.notify_all();
    }
}

#[derive(Clone)]
pub struct TexturePool {
    shared: Arc<Shared>,
    _closer: Arc<Closer>,
}

impl TexturePool {
    /// Queue a job. Viewer pictures go to the front, so the picture on screen
    /// is never stuck behind a backlog of grid thumbnails.
    pub fn request(&self, job: TextureJob) {
        let mut queue = self.shared.lock();
        match job.kind {
            TexKind::Thumb => queue.jobs.push_back(job),
            TexKind::Preview | TexKind::Full => queue.jobs.push_front(job),
        }
        self.shared.ready.notify_one();
    }

    /// Drop queued thumbnail jobs whose key `keep` no longer wants, and return
    /// their keys. Jobs a worker has already started are not touched.
    pub fn retain_thumbs(&self, keep: impl Fn(&str) -> bool) -> Vec<String> {
        let mut dropped = Vec::new();
        self.shared.lock().jobs.retain(|job| {
            let wanted = job.kind != TexKind::Thumb || keep(&job.key);
            if !wanted {
                dropped.push(job.key.clone());
            }
            wanted
        });
        dropped
    }
}

/// Four workers decoding previews in parallel. The results are uploaded on
/// the UI thread (Vulkan is not thread-safe here); only raw pixels cross.
pub fn texture_pool(
    library: Arc<Library>,
    events: Sender<AppEvent>,
    proxy: EventLoopProxy<()>,
    workers: usize,
) -> TexturePool {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            jobs: VecDeque::new(),
            closed: false,
        }),
        ready: Condvar::new(),
    });
    for _ in 0..workers.max(1) {
        let library = Arc::clone(&library);
        let events = events.clone();
        let shared = Arc::clone(&shared);
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("forbiddenlibrary-preview".into())
            .spawn(move || {
                while let Some(job) = shared.next() {
                    let result = match job.kind {
                        TexKind::Thumb => library
                            .load_thumb_rgba(&job.item)
                            .map_err(|e| e.to_string()),
                        TexKind::Preview => library
                            .load_preview_rgba(&job.item, job.max_side)
                            .map_err(|e| e.to_string()),
                        TexKind::Full => library
                            .load_full_rgba(&job.item, archive_core::FULL_SIDE)
                            .map_err(|e| e.to_string()),
                    };
                    // A thumbnail that will not load is rebuilt once: the
                    // cached preview may be missing or half-written, and the
                    // owner of the file is the only one that can fix it.
                    let rebuild = job.kind == TexKind::Thumb && result.is_err();
                    let _ = events.send(AppEvent::Texture {
                        key: job.key,
                        id: job.item.id.clone(),
                        kind: job.kind,
                        epoch: job.epoch,
                        result,
                    });
                    if rebuild {
                        // A failed load with a missing/corrupt preview: ask
                        // the engine to rebuild it, then the UI retries.
                        let result = library.make_thumbnail(&job.item).map_err(|e| e.to_string());
                        let _ = events.send(AppEvent::ThumbRebuilt {
                            id: job.item.id.clone(),
                            result,
                        });
                    }
                    let _ = proxy.send_event(());
                }
            })
            .ok();
    }
    TexturePool {
        shared: Arc::clone(&shared),
        _closer: Arc::new(Closer(shared)),
    }
}

pub fn start_import(
    library: Arc<Library>,
    paths: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    optimize: Option<OptimizeConfig>,
    events: Sender<AppEvent>,
    proxy: EventLoopProxy<()>,
    staging: Option<Arc<tempfile::TempDir>>,
) {
    std::thread::Builder::new()
        .name("forbiddenlibrary-import".into())
        .spawn(move || {
            let _staging = staging;
            let options = ImportOptions {
                cancel: Some(cancel),
                optimize,
                ..Default::default()
            };
            let mut emit = |event: ImportEvent| {
                let _ = events.send(AppEvent::Import(event));
                let _ = proxy.send_event(());
            };
            let report = import_paths(&library, &paths, &options, &mut emit);
            let _ = events.send(AppEvent::ImportDone(report));
            let _ = proxy.send_event(());
        })
        .ok();
}

/// Takes an import back: removes the rows it added, and their files.
pub fn start_discard(
    library: Arc<Library>,
    ids: Vec<String>,
    events: Sender<AppEvent>,
    proxy: EventLoopProxy<()>,
) {
    std::thread::Builder::new()
        .name("forbiddenlibrary-discard".into())
        .spawn(move || {
            let result = library
                .delete_forever(&ids)
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = events.send(AppEvent::ImportDiscarded(result));
            let _ = proxy.send_event(());
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool with no workers, so the queue can be inspected directly.
    fn idle_pool() -> TexturePool {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        });
        TexturePool {
            shared: Arc::clone(&shared),
            _closer: Arc::new(Closer(shared)),
        }
    }

    fn one_photo() -> (tempfile::TempDir, Item) {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let path = source.join("photo.png");
        std::fs::write(&path, dream::png::encode(8, 8, &[128u8; 8 * 8 * 4])).unwrap();
        let library = Library::open(temp.path().join("library")).unwrap();
        import_paths(&library, &[path], &ImportOptions::default(), &mut |_| {});
        let item = library
            .items(&archive_core::TimelineFilter::default())
            .unwrap()
            .remove(0);
        (temp, item)
    }

    fn job(kind: TexKind, key: &str, item: &Item) -> TextureJob {
        TextureJob {
            key: key.into(),
            kind,
            item: item.clone(),
            max_side: 0,
            epoch: 0,
        }
    }

    #[test]
    fn pruning_drops_only_queued_thumbnails_nobody_wants() {
        let (_temp, item) = one_photo();
        let pool = idle_pool();
        pool.request(job(TexKind::Thumb, "t:kept", &item));
        pool.request(job(TexKind::Thumb, "t:gone", &item));
        pool.request(job(TexKind::Preview, "p:open", &item));

        let dropped = pool.retain_thumbs(|key| key == "t:kept");
        assert_eq!(dropped, vec!["t:gone".to_string()]);
        let queue = pool.shared.lock();
        assert_eq!(
            queue.jobs.len(),
            2,
            "the preview and the kept thumbnail stay"
        );
    }

    #[test]
    fn viewer_pictures_are_taken_before_grid_thumbnails() {
        let (_temp, item) = one_photo();
        let pool = idle_pool();
        pool.request(job(TexKind::Thumb, "t:first", &item));
        pool.request(job(TexKind::Preview, "p:viewer", &item));
        let next = pool.shared.next().unwrap();
        assert_eq!(next.kind, TexKind::Preview);
        assert_eq!(pool.shared.next().unwrap().key, "t:first");
    }

    #[test]
    fn closing_the_last_handle_stops_the_workers() {
        let pool = idle_pool();
        let shared = Arc::clone(&pool.shared);
        let clone = pool.clone();
        drop(pool);
        assert!(!shared.lock().closed, "a clone still keeps the queue open");
        drop(clone);
        assert!(shared.next().is_none());
    }
}
