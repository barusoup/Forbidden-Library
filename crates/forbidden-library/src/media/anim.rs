//! Animated GIF, APNG and WebP playback, driven by the viewer's event loop.
//!
//! Decoding runs on a loader thread that stays a few frames ahead. The viewer
//! polls for the frame that is due, and the player decides when that is. The
//! player's other job is to say which frames are worth keeping on the GPU.
//!
//! The first time through, every frame arrives with its pixels. When they all
//! fit the GPU budget the animation is cached: later loops report only indices,
//! and the viewer draws from the textures it already has. When they do not fit,
//! the animation streams instead. Each loop is decoded again from the start and
//! nothing is marked as kept, so a very large animation loops without taking
//! over video memory.

use super::{DecodedFrame, Wake};
use archive_core::AnimFrame;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

/// Frames the loader may decode ahead of the viewer.
const QUEUE_DEPTH: usize = 3;
/// A frame this far behind schedule is shown at the current time rather than
/// caught up on. An app that was minimised should resume on the current frame,
/// not race through the gap.
const RESYNC_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq)]
pub enum AnimStatus {
    Loading,
    Playing,
    Paused,
    NotAnimated,
    Failed(String),
}

pub struct AnimTick {
    /// Position in the loop, from 0.
    pub index: usize,
    /// Pixels for this frame. `None` in cached mode, where the viewer already
    /// holds the texture for `index`.
    pub frame: Option<DecodedFrame>,
    /// Whether the viewer should keep this frame's texture for later loops.
    pub keep: bool,
}

/// What the loader thread sends. `EndPass` marks the end of one full decode
/// of the file, and the loader then starts the next pass by itself.
enum Msg {
    NotAnimated,
    Frame(AnimFrame),
    EndPass,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The first pass is still arriving, with pixels.
    First,
    /// Every frame fit the budget: later loops come from the cache.
    Cached,
    /// Some frame did not fit: each loop is decoded again, with no kept frames.
    Streaming,
}

struct Pending {
    frame: AnimFrame,
    keep: bool,
}

pub struct AnimPlayer {
    rx: Option<Receiver<Msg>>,
    /// Messages sent but not yet received. The loader wakes the app only when
    /// this goes from zero to one, so a burst of frames causes one wake-up.
    queued: Arc<AtomicUsize>,
    gpu_budget: u64,
    mode: Mode,
    cum_bytes: u64,
    all_kept: bool,
    /// Delays of the first pass, used to time cached loops.
    delays: Vec<u32>,
    /// Index of the frame that the next tick will show.
    index: usize,
    /// The next frame, decoded and waiting for its due time.
    next: Option<Pending>,
    /// When the next frame is due. `None` means show as soon as possible.
    due: Option<Instant>,
    last_delay: u32,
    shown: bool,
    paused: bool,
    /// A single-frame animation shows once and then stops.
    idle: bool,
    not_animated: bool,
    failure: Option<String>,
}

impl AnimPlayer {
    /// Start loading `path` in the background. Playback starts as soon as the
    /// first frame arrives. `max_side` bounds the long edge of each frame, and
    /// `gpu_budget` bounds the bytes of the first pass the viewer may keep.
    pub fn open(path: PathBuf, max_side: u32, gpu_budget: u64, wake: Wake) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        let queued = Arc::new(AtomicUsize::new(0));
        let loader_queued = Arc::clone(&queued);
        std::thread::spawn(move || {
            run_loader(path, max_side, sender, loader_queued, wake);
        });
        Self {
            rx: Some(receiver),
            queued,
            gpu_budget,
            mode: Mode::First,
            cum_bytes: 0,
            all_kept: true,
            delays: Vec::new(),
            index: 0,
            next: None,
            due: None,
            last_delay: 0,
            shown: false,
            paused: false,
            idle: false,
            not_animated: false,
            failure: None,
        }
    }

    pub fn status(&self) -> AnimStatus {
        if self.not_animated {
            AnimStatus::NotAnimated
        } else if let Some(message) = &self.failure {
            AnimStatus::Failed(message.clone())
        } else if self.paused {
            AnimStatus::Paused
        } else if self.shown {
            AnimStatus::Playing
        } else {
            AnimStatus::Loading
        }
    }

    /// Resume from the frame on screen. That frame keeps its full delay before
    /// the next one, so a resume never flashes ahead of the pause.
    pub fn play(&mut self, now: Instant) {
        if !self.paused {
            return;
        }
        self.paused = false;
        if self.shown {
            self.due = Some(now + Duration::from_millis(u64::from(self.last_delay)));
        }
    }

    /// Hold on the frame on screen. The clock is not needed here, since the
    /// next due time is recomputed on resume; `_now` keeps the signature the
    /// same as [`play`](Self::play).
    pub fn pause(&mut self, _now: Instant) {
        self.paused = true;
    }

    pub fn toggle(&mut self, now: Instant) {
        if self.paused {
            self.play(now);
        } else {
            self.pause(now);
        }
    }

    /// The frame due at `now`, if any. Returns `None` while waiting for the
    /// next frame to decode or for its due time.
    pub fn poll(&mut self, now: Instant) -> Option<AnimTick> {
        self.fill_next();
        if self.paused || self.idle || self.not_animated || self.failure.is_some() {
            return None;
        }
        let (delay, cached) = match self.mode {
            Mode::Cached => (self.delays[self.index], true),
            _ => (self.next.as_ref()?.frame.delay_ms, false),
        };
        let due = self.due.unwrap_or(now);
        if now < due {
            return None;
        }
        let due = if now.saturating_duration_since(due) > RESYNC_AFTER {
            now
        } else {
            due
        };
        self.due = Some(due + Duration::from_millis(u64::from(delay)));
        self.last_delay = delay;
        self.shown = true;
        let index = self.index;
        if cached {
            self.index = (self.index + 1) % self.delays.len();
            return Some(AnimTick {
                index,
                frame: None,
                keep: true,
            });
        }
        let pending = self.next.take()?;
        self.index += 1;
        Some(AnimTick {
            index,
            frame: Some(DecodedFrame {
                width: pending.frame.width,
                height: pending.frame.height,
                rgba: pending.frame.rgba,
            }),
            keep: pending.keep,
        })
    }

    /// Pixel buffers are dropped rather than pooled. Animation frames differ
    /// in size, and a pool that matches them is not worth its bookkeeping.
    pub fn recycle(&mut self, rgba: Vec<u8>) {
        drop(rgba);
    }

    /// When `poll` should run next. `None` means nothing is scheduled, and the
    /// loader's wake-up is what brings the next frame.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        if self.paused || self.idle || self.not_animated || self.failure.is_some() {
            return None;
        }
        let due = match self.mode {
            Mode::Cached => self.due,
            _ if self.next.is_some() => self.due,
            _ => None,
        }?;
        Some(due.max(now))
    }

    /// Pull at most one message while nothing is waiting to be shown. Keeping
    /// the queue shallow here is what lets the loader's back-pressure limit
    /// memory when the viewer is slow.
    fn fill_next(&mut self) {
        while self.next.is_none() && self.mode != Mode::Cached {
            let Some(receiver) = self.rx.as_ref() else {
                return;
            };
            let msg = match receiver.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            };
            self.queued.fetch_sub(1, Ordering::AcqRel);
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::NotAnimated => {
                self.not_animated = true;
                self.rx = None;
            }
            Msg::Failed(message) => {
                self.failure = Some(message);
                self.rx = None;
            }
            Msg::Frame(frame) => {
                let keep = if self.mode == Mode::First {
                    // Cumulative, so the first frame that does not fit ends
                    // the kept run for the rest of the pass.
                    self.cum_bytes += frame.rgba.len() as u64;
                    self.delays.push(frame.delay_ms);
                    let keep = self.cum_bytes <= self.gpu_budget;
                    self.all_kept &= keep;
                    keep
                } else {
                    false
                };
                self.next = Some(Pending { frame, keep });
            }
            Msg::EndPass => {
                if self.index == 0 {
                    self.failure = Some("This animation has no frames".into());
                    self.rx = None;
                    return;
                }
                if self.mode == Mode::First {
                    self.mode = if self.all_kept {
                        Mode::Cached
                    } else {
                        Mode::Streaming
                    };
                }
                // A cached loop needs nothing more from the loader, so it is
                // dropped, which also stops the loader's next pass.
                if self.mode == Mode::Cached || self.index == 1 {
                    self.rx = None;
                }
                if self.index == 1 {
                    self.idle = true;
                }
                self.index = 0;
            }
        }
    }
}

/// Decode the file, pass after pass, until the player goes away. A pass that
/// errors after producing frames counts as a complete pass: a truncated
/// animation still plays what it has.
fn run_loader(
    path: PathBuf,
    max_side: u32,
    sender: SyncSender<Msg>,
    queued: Arc<AtomicUsize>,
    wake: Wake,
) {
    let send = |msg: Msg| -> bool {
        let was_empty = queued.fetch_add(1, Ordering::AcqRel) == 0;
        if sender.send(msg).is_err() {
            return false;
        }
        if was_empty {
            wake();
        }
        true
    };
    if !archive_core::is_animated(&path) {
        send(Msg::NotAnimated);
        return;
    }
    loop {
        let frames = match archive_core::frames(&path, max_side) {
            Ok(frames) => frames,
            Err(error) => {
                send(Msg::Failed(error.to_string()));
                return;
            }
        };
        let mut produced = 0usize;
        for item in frames {
            match item {
                Ok(frame) => {
                    produced += 1;
                    if !send(Msg::Frame(frame)) {
                        return;
                    }
                }
                Err(error) => {
                    if produced == 0 {
                        send(Msg::Failed(error.to_string()));
                        return;
                    }
                    break;
                }
            }
        }
        if produced == 0 {
            send(Msg::Failed("This animation has no frames".into()));
            return;
        }
        if !send(Msg::EndPass) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::codecs::png::PngEncoder;
    use image::{Delay, ExtendedColorType, Frame, ImageEncoder, Rgba, RgbaImage};
    use std::fs::File;
    use tempfile::TempDir;

    /// Three frames with delays 50, 100 (written as 0, raised by the
    /// browser rule) and 120 ms.
    fn three_frame_gif(path: &std::path::Path) {
        let colors = [
            ([255, 0, 0, 255], 50),
            ([0, 255, 0, 255], 0),
            ([0, 0, 255, 255], 120),
        ];
        let frames: Vec<Frame> = colors
            .iter()
            .map(|(color, delay_ms)| {
                Frame::from_parts(
                    RgbaImage::from_pixel(48, 32, Rgba(*color)),
                    0,
                    0,
                    Delay::from_numer_denom_ms(*delay_ms, 1),
                )
            })
            .collect();
        GifEncoder::new(File::create(path).unwrap())
            .encode_frames(frames)
            .unwrap();
    }

    fn counting_wake() -> (Wake, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let inner = Arc::clone(&count);
        let wake: Wake = Arc::new(move || {
            inner.fetch_add(1, Ordering::SeqCst);
        });
        (wake, count)
    }

    /// Poll until `count` ticks have arrived, sleeping briefly between polls.
    /// Each tick is paired with the time it was taken at.
    fn collect(player: &mut AnimPlayer, count: usize) -> Vec<(Instant, AnimTick)> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut ticks = Vec::new();
        while ticks.len() < count {
            assert!(
                Instant::now() < deadline,
                "timed out with {} of {count} ticks (status {:?})",
                ticks.len(),
                player.status()
            );
            let now = Instant::now();
            match player.poll(now) {
                Some(tick) => ticks.push((now, tick)),
                None => std::thread::sleep(Duration::from_millis(4)),
            }
        }
        ticks
    }

    fn assert_near(actual: Duration, expected_ms: i64, what: &str) {
        let actual_ms = actual.as_millis() as i64;
        assert!(
            (actual_ms - expected_ms).abs() <= 40,
            "{what}: expected about {expected_ms} ms, got {actual_ms} ms"
        );
    }

    #[test]
    fn a_fitting_gif_is_cached_and_loops_by_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spin.gif");
        three_frame_gif(&path);
        let (wake, _) = counting_wake();
        let mut player = AnimPlayer::open(path, 0, u64::MAX, wake);

        let ticks = collect(&mut player, 8);
        let indices: Vec<usize> = ticks.iter().map(|(_, tick)| tick.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 0, 1, 2, 0, 1]);

        for (_, tick) in &ticks[..3] {
            let frame = tick.frame.as_ref().expect("first pass carries pixels");
            assert_eq!((frame.width, frame.height), (48, 32));
            assert_eq!(frame.rgba.len(), 48 * 32 * 4);
            assert!(tick.keep);
        }
        for (_, tick) in &ticks[3..] {
            assert!(tick.frame.is_none(), "cached loops carry no pixels");
            assert!(tick.keep);
        }

        // Each frame stays for its own delay: 50, 100, 120, then the loop.
        let expected = [50, 100, 120, 50, 100, 120, 50];
        for (pair, expected_ms) in ticks.windows(2).zip(expected) {
            assert_near(
                pair[1].0.duration_since(pair[0].0),
                expected_ms,
                "frame gap",
            );
        }
    }

    #[test]
    fn a_gif_over_the_budget_streams_with_no_kept_frames() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spin.gif");
        three_frame_gif(&path);
        let (wake, _) = counting_wake();
        // Smaller than one 48x32 frame, so nothing fits.
        let mut player = AnimPlayer::open(path, 0, 1, wake);

        let ticks = collect(&mut player, 6);
        let indices: Vec<usize> = ticks.iter().map(|(_, tick)| tick.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 0, 1, 2]);
        for (_, tick) in &ticks {
            assert!(!tick.keep, "streaming never keeps a frame");
            let frame = tick
                .frame
                .as_ref()
                .expect("streaming always carries pixels");
            assert_eq!(frame.rgba.len(), 48 * 32 * 4);
        }
    }

    #[test]
    fn a_still_png_is_not_animated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("still.png");
        let pixels = RgbaImage::from_pixel(16, 16, Rgba([1, 2, 3, 255]));
        PngEncoder::new(File::create(&path).unwrap())
            .write_image(pixels.as_raw(), 16, 16, ExtendedColorType::Rgba8)
            .unwrap();
        let (wake, _) = counting_wake();
        let mut player = AnimPlayer::open(path, 0, u64::MAX, wake);

        let deadline = Instant::now() + Duration::from_secs(15);
        while player.status() != AnimStatus::NotAnimated {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for NotAnimated"
            );
            assert!(player.poll(Instant::now()).is_none());
            std::thread::sleep(Duration::from_millis(4));
        }
        assert_eq!(player.next_wake(Instant::now()), None);
    }

    #[test]
    fn pausing_holds_the_frame_and_resuming_continues_the_loop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spin.gif");
        three_frame_gif(&path);
        let (wake, _) = counting_wake();
        let mut player = AnimPlayer::open(path, 0, u64::MAX, wake);

        let first = collect(&mut player, 2);
        assert_eq!(first[1].1.index, 1);
        player.pause(Instant::now());
        assert_eq!(player.status(), AnimStatus::Paused);

        let until = Instant::now() + Duration::from_millis(400);
        while Instant::now() < until {
            assert!(
                player.poll(Instant::now()).is_none(),
                "paused player ticked"
            );
            std::thread::sleep(Duration::from_millis(4));
        }

        player.play(Instant::now());
        assert_eq!(player.status(), AnimStatus::Playing);
        let resumed = collect(&mut player, 1);
        // The frame on screen keeps its full delay (frame 1, 100 ms) before
        // frame 2 shows, and the index picks up where the pause left it.
        assert_eq!(resumed[0].1.index, 2);
        assert_near(
            resumed[0].0.duration_since(first[1].0),
            100 + 400,
            "resume gap",
        );
    }
}
