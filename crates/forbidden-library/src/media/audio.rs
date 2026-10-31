//! Audio output: a cpal stream fed from a shared ring of decoded samples.
//!
//! The decoder runs on a feeder thread and pushes interleaved f32 samples
//! into [`AudioShared`]. The device callback, on the audio thread, drains the
//! ring. The ring is the only thing the two sides share, and it is guarded by
//! a single mutex so that counting played frames and clearing the ring happen
//! under the same lock. That makes a seek's reset atomic with respect to the
//! callback: no frame can be counted against the old position after the reset.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// How much decoded audio may wait in the ring: 0.75 s. Enough to ride out a
/// slow decoder tick, small enough that a seek never has far to throw away.
const RING_SECONDS_NUMER: u64 = 3;
const RING_SECONDS_DENOM: u64 = 4;

/// The device output. Stays on the thread that opened it: cpal streams are
/// not `Send` on every platform, so only [`AudioShared`] crosses threads.
pub struct AudioOut {
    /// Held only to keep the device stream running; dropping it stops sound.
    _stream: Stream,
    shared: Arc<AudioShared>,
    rate: u32,
    channels: u16,
}

impl AudioOut {
    /// Open the default output device with its default configuration. Returns
    /// `None` when there is no device or the stream cannot be built. Callers
    /// fall back to silence and a wall clock, so this never reports why.
    pub fn open() -> Option<AudioOut> {
        let device = cpal::default_host().default_output_device()?;
        let supported = device.default_output_config().ok()?;
        let format = supported.sample_format();
        let config = supported.config();
        let rate = config.sample_rate.0;
        let channels = config.channels;
        let shared = Arc::new(AudioShared::new(rate, channels));
        let on_error = |error| eprintln!("audio output error: {error}");
        let stream = match format {
            SampleFormat::F32 => {
                let shared = Arc::clone(&shared);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            shared.fill(data, channels);
                        },
                        on_error,
                        None,
                    )
                    .ok()?
            }
            SampleFormat::I16 => {
                let shared = Arc::clone(&shared);
                let mut scratch = Vec::new();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            scratch.resize(data.len(), 0.0);
                            shared.fill(&mut scratch, channels);
                            for (out, sample) in data.iter_mut().zip(&scratch) {
                                *out = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                            }
                        },
                        on_error,
                        None,
                    )
                    .ok()?
            }
            SampleFormat::U16 => {
                let shared = Arc::clone(&shared);
                let mut scratch = Vec::new();
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                            scratch.resize(data.len(), 0.0);
                            shared.fill(&mut scratch, channels);
                            // Unsigned formats are centred on the midpoint, not zero.
                            for (out, sample) in data.iter_mut().zip(&scratch) {
                                let centred = (sample.clamp(-1.0, 1.0) + 1.0) * 0.5;
                                *out = (centred * f32::from(u16::MAX)) as u16;
                            }
                        },
                        on_error,
                        None,
                    )
                    .ok()?
            }
            _ => {
                eprintln!("audio output sample format {format:?} is not supported");
                return None;
            }
        };
        // The stream runs from here on, emitting silence until the shared
        // state says otherwise. Starting it now keeps the open cost off the
        // first play.
        stream.play().ok()?;
        Some(AudioOut {
            _stream: stream,
            shared,
            rate,
            channels,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn shared(&self) -> Arc<AudioShared> {
        Arc::clone(&self.shared)
    }
}

/// State shared between the feeder thread, the UI thread and the device
/// callback.
pub struct AudioShared {
    ring: Mutex<Ring>,
    channels: u16,
    capacity: usize,
    running: AtomicBool,
    /// `f32` bits, so volume can change without taking the ring lock.
    volume: AtomicU32,
    muted: AtomicBool,
}

struct Ring {
    samples: VecDeque<f32>,
    /// Frames the callback has actually output since the last reset.
    played_frames: u64,
    /// Bumped by every reset. A feeder holding an older value must stop.
    generation: u64,
    /// The feeder of `generation` has reached end of stream.
    finished: bool,
}

impl AudioShared {
    pub fn new(rate: u32, channels: u16) -> Self {
        let channels = channels.max(1);
        let samples = u64::from(rate) * u64::from(channels);
        let capacity = (samples * RING_SECONDS_NUMER / RING_SECONDS_DENOM) as usize;
        Self {
            ring: Mutex::new(Ring {
                samples: VecDeque::with_capacity(capacity),
                played_frames: 0,
                generation: 0,
                finished: false,
            }),
            channels,
            capacity,
            running: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
        }
    }

    /// While false the callback writes silence and consumes nothing.
    pub fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::Release);
    }

    /// Output gain, clamped to 0..=1.
    pub fn set_volume(&self, v: f32) {
        let v = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    /// Clear the ring, zero the played-frame count, and start a new generation.
    /// Everything happens under one lock, so the callback either counts a frame
    /// before the reset or not at all after it.
    pub fn reset(&self) -> u64 {
        let mut ring = self.lock();
        ring.samples.clear();
        ring.played_frames = 0;
        ring.finished = false;
        ring.generation += 1;
        ring.generation
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Append `samples` if `generation` is still current. Returns `false` when the
    /// generation is stale, which tells the feeder to exit. The caller keeps
    /// the ring under capacity by checking [`room`](Self::room) first.
    pub fn push(&self, generation: u64, samples: &[f32]) -> bool {
        let mut ring = self.lock();
        if ring.generation != generation {
            return false;
        }
        ring.samples.extend(samples.iter().copied());
        true
    }

    /// Free space in samples (not frames), against the 0.75 s capacity.
    pub fn room(&self) -> usize {
        self.capacity.saturating_sub(self.lock().samples.len())
    }

    pub fn buffered_frames(&self) -> usize {
        self.lock().samples.len() / usize::from(self.channels)
    }

    pub fn played_frames(&self) -> u64 {
        self.lock().played_frames
    }

    /// Mark the feed for `generation` as ended. A stale generation is ignored, so a
    /// feeder from before a seek cannot mark the new stream finished.
    pub fn set_finished(&self, generation: u64) {
        let mut ring = self.lock();
        if ring.generation == generation {
            ring.finished = true;
        }
    }

    pub fn finished(&self) -> bool {
        self.lock().finished
    }

    /// Fill `out` from the ring. This is the device callback's body, kept here
    /// so the logic can be tested without a device. Shortfalls are zeroed, and
    /// played frames are counted under the same lock as the pop.
    pub fn fill(&self, out: &mut [f32], channels: u16) {
        if !self.running.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }
        let muted = self.muted.load(Ordering::Relaxed);
        let gain = if muted {
            0.0
        } else {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        };
        let mut ring = self.lock();
        let take = out.len().min(ring.samples.len());
        for (slot, sample) in out[..take].iter_mut().zip(ring.samples.drain(..take)) {
            *slot = sample * gain;
        }
        out[take..].fill(0.0);
        ring.played_frames += (take / usize::from(channels.max(1))) as u64;
    }

    /// The lock is only ever held for a few microseconds, so poisoning is
    /// recovered from rather than propagated into the audio thread.
    fn lock(&self) -> MutexGuard<'_, Ring> {
        self.ring
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 Hz stereo: capacity is 0.75 s = 750 frames = 1500 samples.
    fn shared() -> AudioShared {
        AudioShared::new(1000, 2)
    }

    #[test]
    fn capacity_is_three_quarters_of_a_second() {
        let s = shared();
        assert_eq!(s.room(), 1500);
        assert!(s.push(s.generation(), &[0.5; 1000]));
        assert_eq!(s.room(), 500);
        assert_eq!(s.buffered_frames(), 500);
    }

    #[test]
    fn reset_bumps_the_generation_and_clears_everything() {
        let s = shared();
        let generation = s.generation();
        s.push(generation, &[0.1; 200]);
        s.set_running(true);
        let mut out = [0.0; 200];
        s.fill(&mut out, 2);
        s.set_finished(generation);

        let next = s.reset();
        assert_eq!(next, generation + 1);
        assert_eq!(s.generation(), next);
        assert_eq!(s.buffered_frames(), 0);
        assert_eq!(s.played_frames(), 0);
        assert!(!s.finished());
    }

    #[test]
    fn a_stale_generation_cannot_push() {
        let s = shared();
        let old = s.generation();
        let new = s.reset();
        assert!(!s.push(old, &[1.0; 4]));
        assert_eq!(s.buffered_frames(), 0);
        assert!(s.push(new, &[1.0; 4]));
        assert_eq!(s.buffered_frames(), 2);
    }

    #[test]
    fn a_stale_feeder_cannot_mark_the_new_stream_finished() {
        let s = shared();
        let old = s.generation();
        let new = s.reset();
        s.set_finished(old);
        assert!(!s.finished());
        s.set_finished(new);
        assert!(s.finished());
    }

    #[test]
    fn stopped_output_is_silence_and_consumes_nothing() {
        let s = shared();
        s.push(s.generation(), &[0.5; 8]);
        let mut out = [9.0; 8];
        s.fill(&mut out, 2);
        assert_eq!(out, [0.0; 8]);
        assert_eq!(s.buffered_frames(), 4);
        assert_eq!(s.played_frames(), 0);
    }

    #[test]
    fn running_output_consumes_and_counts_frames() {
        let s = shared();
        s.push(s.generation(), &[0.25; 8]);
        s.set_running(true);
        let mut out = [0.0; 4];
        s.fill(&mut out, 2);
        assert_eq!(out, [0.25; 4]);
        assert_eq!(s.played_frames(), 2);
        assert_eq!(s.buffered_frames(), 2);
    }

    #[test]
    fn a_shortfall_is_zeroed_and_only_real_frames_are_counted() {
        let s = shared();
        s.push(s.generation(), &[0.5; 4]);
        s.set_running(true);
        let mut out = [7.0; 8];
        s.fill(&mut out, 2);
        assert_eq!(out, [0.5, 0.5, 0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(s.played_frames(), 2);
        assert_eq!(s.buffered_frames(), 0);
    }

    #[test]
    fn volume_and_mute_scale_the_output() {
        let s = shared();
        s.set_running(true);
        s.push(s.generation(), &[1.0; 12]);
        s.set_volume(0.5);
        let mut out = [0.0; 4];
        s.fill(&mut out, 2);
        assert_eq!(out, [0.5; 4]);

        s.set_muted(true);
        let mut out = [0.0; 4];
        s.fill(&mut out, 2);
        assert_eq!(out, [0.0; 4]);
        // Muting still consumes: the position keeps moving under mute.
        assert_eq!(s.played_frames(), 4);
    }

    #[test]
    fn volume_is_clamped_and_rejects_non_finite_values() {
        let s = shared();
        s.set_volume(3.0);
        assert_eq!(f32::from_bits(s.volume.load(Ordering::Relaxed)), 1.0);
        s.set_volume(-1.0);
        assert_eq!(f32::from_bits(s.volume.load(Ordering::Relaxed)), 0.0);
        s.set_volume(f32::NAN);
        assert_eq!(f32::from_bits(s.volume.load(Ordering::Relaxed)), 0.0);
    }

    #[test]
    fn played_frames_accumulate_across_callbacks() {
        let s = shared();
        s.set_running(true);
        s.push(s.generation(), &[0.0; 40]);
        let mut out = [0.0; 10];
        for _ in 0..4 {
            s.fill(&mut out, 2);
        }
        assert_eq!(s.played_frames(), 20);
    }
}
