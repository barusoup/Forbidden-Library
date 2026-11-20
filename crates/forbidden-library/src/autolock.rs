//! The idle auto-lock, off the UI thread.
//!
//! Collections opened with a password seal themselves after a minute without
//! input. The UI can only do that while it draws frames, and a minimized or
//! covered window draws none (Dream's runner parks a hidden window at 0 Hz),
//! so this countdown lives on a thread of its own: it seals when the minute
//! runs out and posts the result to the UI, which catches up on its next
//! frame. While the window is visible the UI and this thread share the same
//! timer; whichever gets there first locks, and the other finds nothing left
//! to lock.

use crate::worker::{AppEvent, VaultDone, VaultKind, Wake};
use archive_core::Library;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long every unlocked collection may stay open without input.
pub const VAULT_IDLE: Duration = Duration::from_secs(60);
/// How often the guard looks again at a timer the UI is holding.
const HELD_RECHECK: Duration = Duration::from_secs(1);

struct Shared {
    /// When input last arrived.
    last: Mutex<Instant>,
    changed: Condvar,
    stop: AtomicBool,
    /// The UI is showing something that holds the timer: a picture from an
    /// unlocked collection, or a lock job already running.
    hold: AtomicBool,
    /// How long quiet counts as idle. See [`AutoLock::start`].
    idle: Duration,
    wake: Wake,
    library: Arc<Library>,
    events: Sender<AppEvent>,
}

/// A guard thread that seals unlocked collections when the library goes idle.
pub struct AutoLock {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AutoLock {
    pub fn start(library: Arc<Library>, events: Sender<AppEvent>, wake: Wake) -> Self {
        Self::start_with_idle(library, events, wake, VAULT_IDLE)
    }

    /// The same guard on a custom countdown, which is how the tests exercise
    /// it without waiting a minute.
    fn start_with_idle(
        library: Arc<Library>,
        events: Sender<AppEvent>,
        wake: Wake,
        idle: Duration,
    ) -> Self {
        let shared = Arc::new(Shared {
            last: Mutex::new(Instant::now()),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            hold: AtomicBool::new(false),
            idle,
            wake,
            library,
            events,
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("forbiddenlibrary-autolock".into())
            .spawn(move || guard(worker))
            .ok();
        Self { shared, thread }
    }

    /// Input happened: the countdown starts again.
    pub fn touch(&self) {
        if let Ok(mut last) = self.shared.last.lock() {
            *last = Instant::now();
            self.shared.changed.notify_all();
        }
    }

    /// The UI is holding the timer (see [`Shared::hold`]).
    pub fn hold(&self, held: bool) {
        self.shared.hold.store(held, Ordering::Relaxed);
    }
}

impl Drop for AutoLock {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Wait out the idle time, then seal. Repeats until the app is gone.
fn guard(shared: Arc<Shared>) {
    loop {
        {
            // Sleep until the deadline, or until input moves it.
            let mut last = match shared.last.lock() {
                Ok(last) => last,
                Err(_) => return,
            };
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                let now = Instant::now();
                let due = *last + shared.idle;
                if now >= due {
                    break;
                }
                match shared.changed.wait_timeout(last, due - now) {
                    Ok((next, _)) => last = next,
                    Err(_) => return,
                }
            }
        }
        // The countdown ran out. A picture kept on screen from a collection
        // suspends it, as does a lock already in progress; look again later.
        if shared.hold.load(Ordering::Relaxed) {
            if wait_or_stop(&shared, HELD_RECHECK) {
                return;
            }
            if let Ok(mut last) = shared.last.lock() {
                *last = Instant::now();
                drop(last);
                shared.changed.notify_all();
            }
            continue;
        }
        match shared.library.lock_all() {
            // Nothing was open, so there is nothing to tell the UI about.
            Ok(0) => {}
            locked => {
                let _ = shared.events.send(AppEvent::Vault(VaultDone {
                    collection: 0,
                    kind: VaultKind::LockAll,
                    result: locked.map(|_| ()),
                }));
                shared.wake.wake();
            }
        }
        // Nothing is unlocked, or the seal failed and was reported: wait for
        // the next input before trying again.
        if let Ok(mut last) = shared.last.lock() {
            *last = Instant::now();
        }
    }
}

/// Wait for `span`, or until the app is stopping. Returns `true` to stop.
fn wait_or_stop(shared: &Shared, span: Duration) -> bool {
    let Ok(last) = shared.last.lock() else {
        return true;
    };
    match shared.changed.wait_timeout(last, span) {
        Ok(_) => shared.stop.load(Ordering::Relaxed),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A guard with a wake channel that can be watched, and its receive end.
    fn guard_with_idle(
        library: Arc<Library>,
        idle: Duration,
    ) -> (AutoLock, mpsc::Receiver<AppEvent>, mpsc::Receiver<()>) {
        let (events, received) = mpsc::channel();
        let (woken, wake_seen) = mpsc::channel();
        let guard = AutoLock::start_with_idle(
            library,
            events,
            Wake::new(move || {
                let _ = woken.send(());
            }),
            idle,
        );
        (guard, received, wake_seen)
    }

    #[test]
    fn the_guard_seals_an_unlocked_collection_once_the_idle_time_ends() {
        let temp = tempfile::TempDir::new().unwrap();
        let library = Arc::new(Library::open(temp.path().join("library")).unwrap());
        let id = library.create_collection("Private").unwrap();
        library.create_lock(id, "correct horse battery").unwrap();
        library.unlock(id, "correct horse battery").unwrap();
        assert!(library.is_unlocked(id));

        let (guard, received, wake_seen) =
            guard_with_idle(Arc::clone(&library), Duration::from_millis(80));
        match received.recv_timeout(Duration::from_secs(30)) {
            Ok(AppEvent::Vault(done)) => {
                assert_eq!(done.kind, VaultKind::LockAll);
                done.result.expect("the seal should succeed");
            }
            Ok(_) => panic!("the guard sent something other than a vault result"),
            Err(error) => panic!("the guard never sealed the collection: {error}"),
        }
        assert!(!library.is_unlocked(id), "the collection is sealed again");
        wake_seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the UI is woken to show the seal");
        drop(guard);
    }

    #[test]
    fn an_idle_library_with_nothing_unlocked_is_not_reported() {
        let temp = tempfile::TempDir::new().unwrap();
        let library = Arc::new(Library::open(temp.path().join("library")).unwrap());
        let (guard, received, _wake_seen) = guard_with_idle(library, Duration::from_millis(50));
        assert!(
            received.recv_timeout(Duration::from_millis(500)).is_err(),
            "nothing was open, so the UI must not be told anything"
        );
        drop(guard);
    }

    #[test]
    fn input_keeps_the_guard_waiting() {
        let temp = tempfile::TempDir::new().unwrap();
        let library = Arc::new(Library::open(temp.path().join("library")).unwrap());
        let id = library.create_collection("Private").unwrap();
        library.create_lock(id, "correct horse battery").unwrap();
        library.unlock(id, "correct horse battery").unwrap();

        let (guard, received, _wake_seen) =
            guard_with_idle(Arc::clone(&library), Duration::from_millis(150));
        // Keep touching for longer than the idle time; the seal must wait.
        for _ in 0..6 {
            guard.touch();
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(library.is_unlocked(id), "input keeps the collection open");
        // Then let it go quiet: the countdown restarts and fires.
        let event = received
            .recv_timeout(Duration::from_secs(30))
            .expect("the guard seals after the input stops");
        assert!(matches!(event, AppEvent::Vault(_)));
        assert!(!library.is_unlocked(id));
        drop(guard);
    }
}
