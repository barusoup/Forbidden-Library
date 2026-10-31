//! Video playback: ffmpeg decodes frames and audio, and this player decides
//! which decoded frame is due at each moment.
//!
//! The clock is the authority on time. When audio is running, the clock is
//! re-anchored to what the sound card has actually played, so picture follows
//! sound. Otherwise it runs on the wall clock. A frame is shown only when its
//! timestamp has come up, and a player that falls behind drops the frames it
//! missed rather than playing them late.
//!
//! Each start position is a "session": one ffmpeg for video and one for audio,
//! started at that position. A seek or a restart ends the session and starts a
//! new one, and the old decoders are killed on a throwaway thread so that a
//! slow process exit never stalls the UI.

use super::audio::{AudioOut, AudioShared};
use super::clock::Clock;
use super::{DecodedFrame, Wake};
use archive_core::{
    PlaybackInfo, Stream, Tools, VideoOut, fit_even, playback_info, spawn_audio, spawn_video,
};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::ChildStdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// Largest frame the player will decode, whatever the viewer asks for.
const MAX_DECODE_W: u32 = 3840;
const MAX_DECODE_H: u32 = 2160;
/// Decoded frames a session may hold ahead of the viewer, bounded by bytes so
/// that a 4K stream does not buffer more than the 64 MiB this allows.
const SESSION_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const SESSION_DEPTH_MIN: usize = 2;
const SESSION_DEPTH_MAX: usize = 8;
/// Seeks that arrive closer together than this are coalesced into one, so a
/// scrub across the timeline does not start a decoder for every mouse move.
const SEEK_COALESCE: Duration = Duration::from_millis(120);
/// Audio that must be buffered before a session starts its clock.
const PRIME_AUDIO_MS: usize = 60;
/// Longest a session waits for audio to buffer before starting anyway.
const PRIME_TIMEOUT: Duration = Duration::from_millis(750);
/// A session that produces no frame this long has failed.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
/// An exact seek decodes everything before the position, so it gets longer.
const EXACT_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
/// While audio runs, the player checks this often for drain and end-of-stream,
/// since the audio side has no wake-up of its own.
const AUDIO_POLL: Duration = Duration::from_millis(10);
/// How often a picture-less tail of audio is checked for its end.
const AUDIO_TAIL_POLL: Duration = Duration::from_millis(50);
/// Longest decoder message shown to the user.
const STDERR_LIMIT: usize = 200;

const NO_FFMPEG: &str = "Video playback needs FFmpeg, which was not found";
const NO_AUDIO_DEVICE: &str = "No audio output device";
const NO_FRAME: &str = "This video could not be decoded";

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerStatus {
    Loading,
    Playing,
    Paused,
    Ended,
    Failed(String),
}

/// What the reader thread sends. Every message counts toward the queue depth,
/// including `End`, so the viewer's wake-up logic sees all of them.
enum Msg {
    Frame { pts_ms: i64, rgba: Vec<u8> },
    End,
}

/// One start position's decoders and the state that goes with them.
struct Session {
    start_ms: i64,
    video: Option<Stream>,
    audio: Option<Stream>,
    /// Frames from the reader thread. `None` when the file has no video.
    frames: Option<Receiver<Msg>>,
    /// Hands spent frame buffers back to the reader for reuse.
    recycle: Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    /// The next frame, held until its timestamp comes due.
    pending: Option<(i64, Vec<u8>)>,
    got_frame: bool,
    /// The reader has reached end of stream (or there is no video to read).
    ended: bool,
    hwaccel: bool,
    /// This session seeks by decoding from the start (see `VideoOut`).
    exact: bool,
    /// When the session started. The seek coalescing and the first-frame
    /// timeout are both measured from here.
    started_at: Instant,
    /// The session has buffered enough to start its clock.
    primed: bool,
    audio_running: bool,
    /// Played-frame count the clock was last anchored to.
    last_played: u64,
}

impl Session {
    fn first_frame_timeout(&self) -> Duration {
        if self.exact {
            EXACT_FIRST_FRAME_TIMEOUT
        } else {
            FIRST_FRAME_TIMEOUT
        }
    }

    /// Take one message from the reader without waiting.
    fn pull(&mut self) -> Option<(i64, Vec<u8>)> {
        let receiver = self.frames.as_ref()?;
        match receiver.try_recv() {
            Ok(msg) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                match msg {
                    Msg::Frame { pts_ms, rgba } => {
                        self.got_frame = true;
                        Some((pts_ms, rgba))
                    }
                    Msg::End => {
                        self.ended = true;
                        None
                    }
                }
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.ended = true;
                None
            }
        }
    }
}

enum State {
    /// The probe thread has not reported yet.
    Probing(Receiver<Result<(Tools, PlaybackInfo), String>>),
    Ready,
    Failed(String),
}

pub struct VideoPlayer {
    source: PathBuf,
    max: (u32, u32),
    wake: Wake,
    audio_wanted: bool,
    state: State,
    tools: Option<Tools>,
    info: Option<PlaybackInfo>,
    /// Size of the decoded frames, after fitting to the viewer's bounds.
    decode: (u32, u32),
    audio: Option<AudioOut>,
    clock: Clock,
    /// What the user asked for. Priming and seeking consult this, so a
    /// session that finishes priming later still starts in the right state.
    user_playing: bool,
    ended: bool,
    session: Option<Session>,
    /// A seek that has been asked for but not yet applied.
    want: Option<i64>,
    /// Starts true, and drops to false for good once hardware decoding fails.
    hwaccel: bool,
    /// The next session seeks exactly. Set only for the one retry after a
    /// demuxer seek decoded nothing.
    exact_once: bool,
    volume: f32,
    muted: bool,
}

impl VideoPlayer {
    /// Returns immediately. Probing runs on a thread, and `wake` is called when
    /// it finishes. Playback starts by itself once the file is ready.
    pub fn open(source: PathBuf, max_w: u32, max_h: u32, wake: Wake, audio: bool) -> Self {
        let (sender, receiver) = mpsc::channel();
        let probe_source = source.clone();
        let probe_wake = Arc::clone(&wake);
        std::thread::spawn(move || {
            let _ = sender.send(probe(&probe_source));
            probe_wake();
        });
        Self {
            source,
            max: (max_w, max_h),
            wake,
            audio_wanted: audio,
            state: State::Probing(receiver),
            tools: None,
            info: None,
            decode: (0, 0),
            audio: None,
            clock: Clock::new(0),
            user_playing: true,
            ended: false,
            session: None,
            want: None,
            hwaccel: true,
            exact_once: false,
            volume: 1.0,
            muted: false,
        }
    }

    pub fn status(&self) -> PlayerStatus {
        match &self.state {
            State::Probing(_) => PlayerStatus::Loading,
            State::Failed(message) => PlayerStatus::Failed(message.clone()),
            State::Ready if self.ended => PlayerStatus::Ended,
            State::Ready if self.user_playing => PlayerStatus::Playing,
            State::Ready => PlayerStatus::Paused,
        }
    }

    pub fn info(&self) -> Option<&PlaybackInfo> {
        self.info.as_ref()
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.info.as_ref()?.duration_ms
    }

    /// The requested position while a seek is pending, so the scrub bar does
    /// not snap back to where the old session was.
    pub fn position_ms(&self, now: Instant) -> i64 {
        self.want.unwrap_or_else(|| self.clock.position(now))
    }

    pub fn play(&mut self, now: Instant) {
        if self.ended {
            // Playing from the end starts again from the beginning.
            self.ended = false;
            self.user_playing = true;
            self.want = None;
            self.begin(0, now);
            return;
        }
        if self.user_playing {
            return;
        }
        self.user_playing = true;
        if matches!(self.state, State::Ready) && self.session.as_ref().is_some_and(|s| s.primed) {
            self.clock.start(now);
            self.set_audio_running(true);
        }
    }

    pub fn pause(&mut self, now: Instant) {
        self.user_playing = false;
        if !matches!(self.state, State::Ready) {
            return;
        }
        // Take the audio position first, so the frame held on screen is the
        // one the listener last heard.
        self.sync_audio_clock(now);
        self.clock.stop(now);
        self.set_audio_running(false);
    }

    pub fn toggle(&mut self, now: Instant) {
        if self.user_playing {
            self.pause(now);
        } else {
            self.play(now);
        }
    }

    /// Clamped to the file's length. Cheap to call on every frame while the
    /// user drags the scrub bar: only the last request is applied, and only
    /// once [`SEEK_COALESCE`] has passed since the last session started.
    pub fn seek(&mut self, ms: i64, _now: Instant) {
        let upper = self.duration_ms().unwrap_or(i64::MAX);
        self.want = Some(ms.clamp(0, upper));
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if let Some(audio) = &self.audio {
            audio.shared().set_volume(self.volume);
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_muted(&mut self, m: bool) {
        self.muted = m;
        if let Some(audio) = &self.audio {
            audio.shared().set_muted(m);
        }
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Advance to `now`. Returns a frame when the one on screen should change.
    pub fn poll(&mut self, now: Instant) -> Option<DecodedFrame> {
        self.check_probe(now);
        if !matches!(self.state, State::Ready) {
            return None;
        }
        self.apply_seek(now);
        self.sync_audio_clock(now);
        let frame = self.select_frame(now);
        self.check_priming(now);
        self.check_end(now);
        frame
    }

    /// Hand a displayed frame's buffer back to the reader for reuse. When the
    /// session is gone the buffer is simply dropped.
    pub fn recycle(&mut self, rgba: Vec<u8>) {
        if let Some(session) = &self.session {
            let _ = session.recycle.send(rgba);
        }
    }

    /// When `poll` should run next. `None` means only the reader's wake-up can
    /// bring the next frame.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        if !matches!(self.state, State::Ready) || self.ended {
            return None;
        }
        let session = self.session.as_ref()?;
        let mut soonest: Option<Instant> = None;
        let mut consider = |at: Instant| {
            let at = at.max(now);
            soonest = Some(soonest.map_or(at, |current| current.min(at)));
        };
        if self.want.is_some() {
            let at = if session.got_frame {
                now
            } else {
                session.started_at + SEEK_COALESCE
            };
            consider(at);
        }
        // Priming checks the audio buffer, so it polls briefly. Once playing,
        // the pending frame's due time paces the UI; audio alone only needs
        // watching when no frame is left to wait for (an audio-only file, or
        // an audio track that outlasts the picture).
        if !session.primed {
            consider(now + AUDIO_POLL);
        } else if session.audio_running && (session.video.is_none() || session.ended) {
            consider(now + AUDIO_TAIL_POLL);
        }
        if session.video.is_some() && !session.got_frame {
            consider(session.started_at + session.first_frame_timeout());
        }
        if let Some((pts_ms, _)) = &session.pending
            && self.clock.running()
        {
            let wait = (*pts_ms - self.clock.position(now)).max(0);
            consider(now + Duration::from_millis(wait as u64));
        }
        soonest
    }

    fn check_probe(&mut self, now: Instant) {
        let State::Probing(receiver) = &self.state else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("The media probe stopped unexpectedly".into()),
        };
        match result {
            Err(message) => self.state = State::Failed(message),
            Ok((tools, info)) => self.become_ready(tools, info, now),
        }
    }

    fn become_ready(&mut self, tools: Tools, info: PlaybackInfo, now: Instant) {
        let audio = if self.audio_wanted && info.has_audio {
            AudioOut::open()
        } else {
            None
        };
        if !info.has_video && audio.is_none() {
            // An audio-only file has nothing to show without a device to play it.
            self.state = State::Failed(NO_AUDIO_DEVICE.into());
            return;
        }
        if let Some(audio) = &audio {
            audio.shared().set_volume(self.volume);
            audio.shared().set_muted(self.muted);
        }
        self.decode = if info.has_video {
            fit_even(
                info.width,
                info.height,
                self.max.0.min(MAX_DECODE_W),
                self.max.1.min(MAX_DECODE_H),
            )
        } else {
            (0, 0)
        };
        self.audio = audio;
        self.tools = Some(tools);
        self.info = Some(info);
        self.state = State::Ready;
        self.begin(0, now);
    }

    /// Start a new session at `start_ms`. The clock is left stopped there and
    /// the session primes itself before any audio or the clock runs.
    fn begin(&mut self, start_ms: i64, now: Instant) {
        self.end_session();
        self.ended = false;
        self.clock = Clock::new(start_ms);
        let exact = std::mem::take(&mut self.exact_once);
        let (Some(info), Some(tools)) = (self.info.clone(), self.tools.clone()) else {
            return;
        };
        let source = self.source.clone();
        let queued = Arc::new(AtomicUsize::new(0));
        let (recycle_tx, recycle_rx) = mpsc::channel();

        let mut video = None;
        let mut frames = None;
        if info.has_video {
            let out = VideoOut {
                width: self.decode.0,
                height: self.decode.1,
                fps: info.fps,
                hwaccel: self.hwaccel,
                exact_seek: exact,
            };
            let mut stream = match spawn_video(&tools, &source, start_ms, &out) {
                Ok(stream) => stream,
                Err(error) => {
                    self.fail(error.to_string());
                    return;
                }
            };
            if let Some(stdout) = stream.stdout() {
                let bytes = self.decode.0 as usize * self.decode.1 as usize * 4;
                frames = Some(spawn_reader(
                    stdout,
                    bytes,
                    start_ms,
                    info.fps,
                    recycle_rx,
                    Arc::clone(&queued),
                    Arc::clone(&self.wake),
                ));
            }
            video = Some(stream);
        }

        let mut audio = None;
        if let (Some(out), true) = (self.audio.as_ref(), info.has_audio) {
            let shared = out.shared();
            shared.set_running(false);
            // The generation is taken before the feeder starts, so the feeder
            // can tell when a later seek has made it stale.
            let generation = shared.reset();
            let channels = out.channels();
            match spawn_audio(&tools, &source, start_ms, out.rate(), channels) {
                Ok(mut stream) => {
                    if let Some(stdout) = stream.stdout() {
                        std::thread::spawn(move || {
                            feed_audio(stdout, shared, generation, channels);
                        });
                        audio = Some(stream);
                    }
                }
                // Without audio the session still plays, on the wall clock.
                Err(error) => eprintln!("audio decode did not start: {error}"),
            }
        }

        let has_video = video.is_some();
        self.session = Some(Session {
            start_ms,
            video,
            audio,
            frames,
            recycle: recycle_tx,
            queued,
            pending: None,
            got_frame: false,
            // Audio-only files have no reader to wait for, so they begin
            // already at end of video.
            ended: !has_video,
            hwaccel: self.hwaccel,
            exact,
            started_at: now,
            primed: false,
            audio_running: false,
            last_played: 0,
        });
    }

    /// Stop the current session. Its streams are dropped on a throwaway thread,
    /// because dropping a stream waits for its process to exit. Bumping the
    /// audio generation makes any feeder still running exit on its next push.
    fn end_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            let streams = (session.video.take(), session.audio.take());
            std::thread::spawn(move || drop(streams));
        }
        if let Some(audio) = &self.audio {
            let shared = audio.shared();
            shared.set_running(false);
            shared.reset();
        }
    }

    fn fail(&mut self, message: String) {
        self.end_session();
        self.state = State::Failed(message);
    }

    /// Turn audio output on or off, for the session and the device together.
    /// Audio only runs when the session has an audio stream to feed it.
    fn set_audio_running(&mut self, on: bool) {
        let has_stream = self.session.as_ref().is_some_and(|s| s.audio.is_some());
        if let Some(session) = self.session.as_mut() {
            session.audio_running = on && has_stream;
        }
        if let Some(audio) = &self.audio {
            audio.shared().set_running(on && has_stream);
        }
    }

    /// Re-anchor the clock to the audio that has actually played. Audio is the
    /// reference because the listener hears it directly. If audio stops, the
    /// clock keeps running on the wall, which is how audio that ends early
    /// lets the video finish.
    fn sync_audio_clock(&mut self, now: Instant) {
        let (Some(session), Some(audio)) = (self.session.as_mut(), self.audio.as_ref()) else {
            return;
        };
        if !session.audio_running {
            return;
        }
        let shared = audio.shared();
        let played = shared.played_frames();
        if played != session.last_played {
            session.last_played = played;
            let rate = u64::from(audio.rate());
            let at_ms = session.start_ms + (played * 1000 / rate) as i64;
            self.clock.set(at_ms, now);
        }
    }

    fn apply_seek(&mut self, now: Instant) {
        let Some(ms) = self.want else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let coalesced = now.saturating_duration_since(session.started_at) >= SEEK_COALESCE;
        if !(session.got_frame || coalesced) {
            return;
        }
        self.want = None;
        self.begin(ms, now);
    }

    /// Start the clock once the session has buffered enough to play smoothly.
    /// A paused session still reaches this point, so its first frame is shown
    /// without starting playback.
    fn check_priming(&mut self, now: Instant) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session.primed {
            return;
        }
        let video_ready = session.video.is_none() || session.got_frame;
        let elapsed = now.saturating_duration_since(session.started_at);
        let audio_ready = match (&session.audio, &self.audio) {
            (Some(_), Some(audio)) => {
                let needed = audio.rate() as usize * PRIME_AUDIO_MS / 1000;
                audio.shared().buffered_frames() >= needed || elapsed >= PRIME_TIMEOUT
            }
            _ => true,
        };
        if !(video_ready && audio_ready) {
            return;
        }
        if let Some(session) = self.session.as_mut() {
            session.primed = true;
        }
        if self.user_playing {
            self.clock.start(now);
            self.set_audio_running(true);
        }
    }

    /// Drain the reader up to the clock. The newest frame that has come due
    /// wins; any frames passed over are recycled, which is how a slow viewer
    /// drops frames instead of falling behind.
    fn select_frame(&mut self, now: Instant) -> Option<DecodedFrame> {
        let position = self.clock.position(now);
        let (width, height) = self.decode;
        let session = self.session.as_mut()?;
        let mut winner: Option<(i64, Vec<u8>)> = None;
        loop {
            if session.pending.is_none() {
                session.pending = session.pull();
            }
            let due = matches!(&session.pending, Some((pts, _)) if *pts <= position);
            if !due {
                break;
            }
            let Some(frame) = session.pending.take() else {
                break;
            };
            if let Some((_, skipped)) = winner.replace(frame) {
                let _ = session.recycle.send(skipped);
            }
        }
        winner.map(|(_, rgba)| DecodedFrame {
            width,
            height,
            rgba,
        })
    }

    /// Detect the end of the file, a session that produced nothing, and a
    /// hardware decoder that gave up.
    fn check_end(&mut self, now: Instant) {
        enum Next {
            Nothing,
            Restart(i64),
            Exact(i64),
            Fail(String),
            End,
        }
        let next = {
            let Some(session) = self.session.as_ref() else {
                return;
            };
            let has_video = session.video.is_some();
            let timed_out = has_video
                && !session.got_frame
                && now.saturating_duration_since(session.started_at)
                    >= session.first_frame_timeout();
            let decoded_nothing = has_video && !session.got_frame && session.ended;
            if timed_out || decoded_nothing {
                // A hardware decoder that produced nothing, or hung, gets one
                // retry in software. After that, nothing from a seek past the
                // last frame is simply the end; nothing from the start is a
                // file that cannot be decoded.
                if session.hwaccel {
                    Next::Restart(session.start_ms)
                } else if decoded_nothing && session.start_ms > 0 && !session.exact {
                    // A demuxer seek can land past the last keyframe a long
                    // GOP stream without an index needs. Try once more,
                    // decoding from the start up to the position.
                    Next::Exact(session.start_ms)
                } else if decoded_nothing && session.start_ms > 0 {
                    Next::End
                } else {
                    Next::Fail(decode_failure(session.video.as_ref()))
                }
            } else {
                let audio_drained = match (&session.audio, &self.audio) {
                    (Some(_), Some(audio)) => {
                        let shared = audio.shared();
                        shared.finished() && shared.buffered_frames() == 0
                    }
                    _ => true,
                };
                if session.ended
                    && session.pending.is_none()
                    && self.clock.running()
                    && audio_drained
                {
                    Next::End
                } else {
                    Next::Nothing
                }
            }
        };
        match next {
            Next::Nothing => {}
            Next::Restart(start_ms) => {
                self.hwaccel = false;
                self.begin(start_ms, now);
            }
            Next::Exact(start_ms) => {
                self.exact_once = true;
                self.begin(start_ms, now);
            }
            Next::Fail(message) => self.fail(message),
            Next::End => {
                self.ended = true;
                self.user_playing = false;
                self.clock.stop(now);
                self.set_audio_running(false);
            }
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.end_session();
    }
}

/// Probe on a helper thread: the ffmpeg tools, then the file's streams.
fn probe(source: &Path) -> Result<(Tools, PlaybackInfo), String> {
    let tools = Tools::discover();
    if !tools.has_ffmpeg() {
        return Err(NO_FFMPEG.into());
    }
    let info = playback_info(&tools, source).map_err(|error| error.to_string())?;
    Ok((tools, info))
}

/// The first line of the decoder's stderr, or a fixed message when it said
/// nothing. A failed decode is often explained in one line, so that line is
/// what the user is shown.
fn decode_failure(video: Option<&Stream>) -> String {
    let text = video.map(Stream::stderr_text).unwrap_or_default();
    match text.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => line.chars().take(STDERR_LIMIT).collect(),
        None => NO_FRAME.into(),
    }
}

/// Read raw RGBA frames from the decoder, timestamp them, and hand them over.
/// The thread stops when the receiver is dropped, which is how a seek or a
/// close ends it.
fn spawn_reader(
    mut stdout: ChildStdout,
    bytes: usize,
    start_ms: i64,
    fps: f64,
    recycled: Receiver<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    wake: Wake,
) -> Receiver<Msg> {
    let depth = (SESSION_BUFFER_BYTES / bytes.max(1)).clamp(SESSION_DEPTH_MIN, SESSION_DEPTH_MAX);
    let (sender, receiver) = mpsc::sync_channel(depth);
    std::thread::spawn(move || {
        let send = |msg: Msg| -> bool {
            // Counted before the send, so the viewer's decrement can never
            // run ahead of this increment.
            let was_empty = queued.fetch_add(1, Ordering::AcqRel) == 0;
            if sender.send(msg).is_err() {
                return false;
            }
            if was_empty {
                wake();
            }
            true
        };
        let mut index: u64 = 0;
        loop {
            let mut rgba = recycled.try_recv().unwrap_or_default();
            rgba.resize(bytes, 0);
            if stdout.read_exact(&mut rgba).is_err() {
                send(Msg::End);
                return;
            }
            // Frame i is at start + i / fps. Computed from the index rather
            // than summed, so rounding never accumulates.
            let pts_ms = start_ms + (index as f64 * 1000.0 / fps).round() as i64;
            index += 1;
            if !send(Msg::Frame { pts_ms, rgba }) {
                return;
            }
        }
    });
    receiver
}

/// Turn the decoder's f32 little-endian bytes into samples and push them into
/// the ring. Partial samples and partial frames are carried over to the next
/// read, so a read boundary never splits a sample.
fn feed_audio(mut stdout: ChildStdout, shared: Arc<AudioShared>, generation: u64, channels: u16) {
    let frame_bytes = 4 * usize::from(channels.max(1));
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let read = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        carry.extend_from_slice(&chunk[..read]);
        let whole = carry.len() / frame_bytes * frame_bytes;
        if whole == 0 {
            continue;
        }
        let samples: Vec<f32> = carry[..whole]
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect();
        carry.drain(..whole);
        if !push_all(&shared, generation, channels, &samples) {
            return;
        }
    }
    shared.set_finished(generation);
}

/// Push `samples` in as much as the ring has room for, waiting in between.
/// Returns `false` once the generation is stale, meaning the session ended.
fn push_all(shared: &AudioShared, generation: u64, channels: u16, mut samples: &[f32]) -> bool {
    let channels = usize::from(channels.max(1));
    while !samples.is_empty() {
        if shared.generation() != generation {
            return false;
        }
        // Only whole frames are pushed, so the callback's frame count stays exact.
        let room = shared.room() / channels * channels;
        if room == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let take = room.min(samples.len());
        if !shared.push(generation, &samples[..take]) {
            return false;
        }
        samples = &samples[take..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn counting_wake() -> (Wake, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let inner = Arc::clone(&count);
        let wake: Wake = Arc::new(move || {
            inner.fetch_add(1, Ordering::SeqCst);
        });
        (wake, count)
    }

    /// Returns the ffmpeg path, or `None` after printing a skip note.
    fn ffmpeg() -> Option<PathBuf> {
        let tools = Tools::discover();
        if tools.has_ffmpeg() && tools.ffprobe.is_some() {
            tools.ffmpeg.clone()
        } else {
            eprintln!("skipped: ffmpeg and ffprobe are not both available");
            None
        }
    }

    /// A two-second 320x240, 25 fps clip with a 440 Hz tone. H.264 when this
    /// build has libx264, MPEG-4 part 2 otherwise. Written with plain ffmpeg
    /// calls, as the archive-core playback tests do.
    fn sample_clip(ffmpeg: &Path, dir: &Path) -> PathBuf {
        let dest = dir.join("clip.mp4");
        for codec in ["libx264", "mpeg4"] {
            let status = Command::new(ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=320x240:rate=25",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440",
                    "-t",
                    "2",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:v",
                    codec,
                    "-c:a",
                    "aac",
                ])
                .arg(&dest)
                .status();
            if status.map(|s| s.success()).unwrap_or(false) {
                return dest;
            }
        }
        panic!("ffmpeg could not write the sample clip");
    }

    /// Poll until `done` is true, or fail after 15 s. Returns the frames seen.
    fn drive(
        player: &mut VideoPlayer,
        mut done: impl FnMut(&VideoPlayer) -> bool,
    ) -> Vec<DecodedFrame> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut frames = Vec::new();
        loop {
            let now = Instant::now();
            if let Some(frame) = player.poll(now) {
                frames.push(frame);
            }
            if done(player) {
                return frames;
            }
            assert!(now < deadline, "timed out in state {:?}", player.status());
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn wait_for_playing(player: &mut VideoPlayer) -> Vec<DecodedFrame> {
        drive(player, |p| {
            p.status() == PlayerStatus::Playing && p.session.as_ref().is_some_and(|s| s.got_frame)
        })
    }

    #[test]
    fn a_clip_plays_to_its_end_at_its_size_and_reports_ended() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 640, 480, wake, false);

        let started = Instant::now();
        let frames = drive(&mut player, |p| p.status() == PlayerStatus::Ended);
        let elapsed = started.elapsed();

        assert!(frames.iter().all(|f| (f.width, f.height) == (320, 240)));
        assert!(frames.iter().all(|f| f.rgba.len() == 320 * 240 * 4));
        assert!(
            (35..=55).contains(&frames.len()),
            "a 2 s clip at 25 fps showed {} frames",
            frames.len()
        );
        assert!(
            elapsed >= Duration::from_millis(1800),
            "ended too early: {elapsed:?}"
        );
        let duration = player.duration_ms().expect("the probe reports a duration");
        assert!(
            (1900..=2100).contains(&duration),
            "duration was {duration} ms"
        );
    }

    #[test]
    fn a_clip_is_downscaled_to_the_bounds_it_is_given() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 160, 120, wake, false);
        let frames = wait_for_playing(&mut player);
        assert!(frames.iter().all(|f| (f.width, f.height) == (160, 120)));
        assert!(frames.iter().all(|f| f.rgba.len() == 160 * 120 * 4));
    }

    #[test]
    fn a_seek_while_paused_shows_a_frame_at_the_position() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 320, 240, wake, false);
        wait_for_playing(&mut player);

        let now = Instant::now();
        player.pause(now);
        player.seek(1000, now);
        let asked = Instant::now();
        // The old session already holds frames, so wait for the seek to be
        // applied (no pending request) and for its first frame to arrive.
        let shown = drive(&mut player, |p| {
            p.want.is_none() && p.session.as_ref().is_some_and(|s| s.got_frame)
        });
        let first_frame_after = asked.elapsed();
        assert!(!shown.is_empty(), "a paused seek must show its first frame");
        assert!(
            first_frame_after < Duration::from_secs(2),
            "seek took {first_frame_after:?} to show a frame"
        );
        assert_eq!(player.status(), PlayerStatus::Paused);
        assert_eq!(player.position_ms(Instant::now()), 1000);
    }

    #[test]
    fn a_seek_in_a_long_gop_transport_stream_still_shows_a_frame() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        // One keyframe for the whole clip, and a container with no index: a
        // demuxer seek lands after the only keyframe and decodes nothing.
        let clip = dir.path().join("long-gop.ts");
        let made = Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25",
                "-t",
                "3",
            ])
            .args(["-pix_fmt", "yuv420p", "-g", "250", "-c:v", "mpeg2video"])
            .arg(&clip)
            .status()
            .is_ok_and(|status| status.success());
        assert!(made, "ffmpeg could not write the transport stream");
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 320, 240, wake, false);
        wait_for_playing(&mut player);

        let now = Instant::now();
        player.pause(now);
        player.seek(1500, now);
        let shown = drive(&mut player, |p| {
            p.want.is_none() && p.session.as_ref().is_some_and(|s| s.got_frame)
        });
        assert!(!shown.is_empty(), "the seek must show a frame, not end");
        assert_eq!(player.status(), PlayerStatus::Paused);
        assert_eq!(player.position_ms(Instant::now()), 1500);
    }

    #[test]
    fn pausing_holds_the_position() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 320, 240, wake, false);
        wait_for_playing(&mut player);
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            player.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(4));
        }

        let now = Instant::now();
        player.pause(now);
        let held = player.position_ms(now);
        let later = now + Duration::from_millis(300);
        assert_eq!(player.position_ms(later), held);
        assert_eq!(player.status(), PlayerStatus::Paused);
        assert!(held > 0, "playback should have advanced before the pause");
    }

    #[test]
    fn play_after_the_end_starts_from_zero() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 320, 240, wake, false);
        drive(&mut player, |p| p.status() == PlayerStatus::Ended);

        player.play(Instant::now());
        assert_eq!(player.status(), PlayerStatus::Playing);
        drive(&mut player, |p| {
            p.session.as_ref().is_some_and(|s| s.got_frame)
        });
        assert!(player.position_ms(Instant::now()) < 200);
    }

    #[test]
    fn a_file_that_is_not_video_fails_within_the_timeout() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("broken.mp4");
        std::fs::write(&path, b"this is not a video at all, just text").unwrap();
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(path, 640, 480, wake, false);
        let started = Instant::now();
        drive(&mut player, |p| {
            matches!(p.status(), PlayerStatus::Failed(_))
        });
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(matches!(player.status(), PlayerStatus::Failed(_)));
    }

    #[test]
    fn dropping_a_playing_player_returns_promptly() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 320, 240, wake, false);
        wait_for_playing(&mut player);
        let started = Instant::now();
        drop(player);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Plays the clip with the real output device. Run by hand with
    /// `cargo test -p forbidden-library audible -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn audible_playback_reaches_the_end_with_a_device() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = TempDir::new().unwrap();
        let clip = sample_clip(&ffmpeg, dir.path());
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(clip, 320, 240, wake, true);
        drive(&mut player, |p| p.status() == PlayerStatus::Playing);
        drive(&mut player, |p| p.status() == PlayerStatus::Ended);
    }

    /// Music has no picture stream, so it must play to its end through the same
    /// engine as video, with no frames. It needs an audio device, and skips
    /// without one.
    #[test]
    fn music_plays_to_its_end_with_no_picture() {
        let Some(ffmpeg) = ffmpeg() else { return };
        if AudioOut::open().is_none() {
            eprintln!("skipped: no audio output device");
            return;
        }
        let dir = TempDir::new().unwrap();
        let track = dir.path().join("tone.mp3");
        let status = Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-c:a", "libmp3lame"])
            .arg(&track)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg could not write the sample track");
        let (wake, _) = counting_wake();
        let mut player = VideoPlayer::open(track, 640, 480, wake, true);
        // Longer than `drive`'s limit: the other media tests share the CPU when
        // the suite runs in parallel, and a two-second track must not time out.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut frames = Vec::new();
        while player.status() != PlayerStatus::Ended {
            if let Some(frame) = player.poll(Instant::now()) {
                frames.push(frame);
            }
            assert!(
                Instant::now() < deadline,
                "timed out in state {:?}",
                player.status()
            );
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(frames.is_empty(), "a music file has no picture to draw");
        let info = player.info().expect("probed before it could end");
        assert!(!info.has_video && info.has_audio);
    }

    #[test]
    fn a_ring_with_a_full_buffer_takes_only_whole_frames() {
        let shared = Arc::new(AudioShared::new(1000, 2));
        let generation = shared.generation();
        let samples = vec![0.25f32; 4000];
        // Room is 1500 samples, so the pusher must wait for the callback.
        std::thread::scope(|scope| {
            let pusher = Arc::clone(&shared);
            scope.spawn(move || {
                assert!(push_all(&pusher, generation, 2, &samples));
            });
            std::thread::sleep(Duration::from_millis(30));
            shared.set_running(true);
            let mut out = vec![0.0f32; 1000];
            let deadline = Instant::now() + Duration::from_secs(5);
            while shared.played_frames() < 2000 && Instant::now() < deadline {
                shared.fill(&mut out, 2);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        assert!(shared.played_frames() >= 2000);
    }
}
