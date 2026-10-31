//! Invocation of the bundled ffmpeg/ffprobe and libjxl (`cjxl`/`djxl`) tools.
//!
//! The installer places an LGPL ffmpeg build in `ffmpeg/` and libjxl's
//! command-line tools in `jxl/` next to the app. A development checkout may
//! set `FORBIDDEN_LIBRARY_FFMPEG_DIR` or `FORBIDDEN_LIBRARY_JXL_DIR`, and a
//! plain `ffmpeg` on `PATH` is accepted as a last resort so tests and
//! developer builds work without a vendor step.
//!
//! Imported files are untrusted. Every call is therefore bounded: one
//! decoding thread, an allocation ceiling, an input-format allowlist and a
//! deadline after which the process tree is killed. On Windows the child is
//! also placed in a job object with a memory and process-count limit, so a
//! decoder that misbehaves cannot take the whole app with it.

use crate::error::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long one media call may run before it is killed.
pub const DEADLINE: Duration = Duration::from_secs(120);
/// How much of a tool's output is kept. Long diagnostics are drained and
/// dropped rather than buffered: a noisy decoder must not grow memory.
const STDOUT_CAP: usize = 4 * 1024 * 1024;
const STDERR_CAP: usize = 64 * 1024;
/// Largest file ffmpeg may write for a video poster. A generated thumbnail is
/// a few hundred kilobytes; anything near this is a sign the scale filter was
/// ignored.
const OUTPUT_CAP: u64 = 32 * 1024 * 1024;
/// Slack on top of a full-size RGBA frame for the PNG container and its
/// row filters. A lossless still is never bigger than its raw pixels plus
/// this.
const STILL_HEADROOM: u64 = 8 * 1024 * 1024;
/// 256 MiB of transient allocation per tool invocation.
const MAX_ALLOC: &str = "268435456";
/// The scaler used for every downscale. Lanczos with accurate rounding and
/// full chroma interpolation keeps a decoded still close to its source; the
/// default bicubic visibly softens fine detail.
const SCALER: &str = "lanczos+accurate_rnd+full_chroma_int";

/// Input demuxers the app accepts. Playlists, concat and other
/// instruction-carrying demuxers are deliberately absent: a "video" must be a
/// media container, not a script that makes FFmpeg open something else.
const FORMATS: &str = "mov,matroska,webm,avi,mpeg,mpegts,flv,ogg,asf,rm,m4v,h264,hevc,av1,ivf,gif,apng,webp,image2,png_pipe,jpeg_pipe,jpegls_pipe,jpegxl_pipe,bmp_pipe,tiff_pipe,webp_pipe,qoi_pipe,exr_pipe,dds_pipe,ico,pnm_pipe,ppm_pipe,pgm_pipe,pam_pipe,mp3,flac,wav,aac,aiff,ape,wv,tta,caf,w64,mpc,mpc8,dsf";
/// A re-encode of a long lossless album can take minutes at high compression,
/// so it gets far more time than a preview.
const TRANSCODE_DEADLINE: Duration = Duration::from_secs(900);

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    /// libjxl's encoder and decoder. JPEG XL needs both: encoding is checked
    /// by decoding the result back.
    pub cjxl: Option<PathBuf>,
    pub djxl: Option<PathBuf>,
}

/// What [`Tools::discover`] found, kept for the life of the process. Every
/// thumbnail and texture load asks for the tools, and each fresh search would
/// otherwise scan `PATH` and canonicalize every candidate.
static DISCOVERED: OnceLock<Tools> = OnceLock::new();

impl Tools {
    /// The tools this process can use. The search runs once and is reused.
    pub fn discover() -> Self {
        DISCOVERED.get_or_init(Self::discover_fresh).clone()
    }

    /// Search again, bypassing the cache. Only code that changes where the
    /// tools are looked for needs this, such as a test that sets an
    /// environment variable after the first search.
    pub fn discover_fresh() -> Self {
        Self {
            ffmpeg: locate("FORBIDDEN_LIBRARY_FFMPEG_DIR", "ffmpeg", "ffmpeg"),
            ffprobe: locate("FORBIDDEN_LIBRARY_FFMPEG_DIR", "ffmpeg", "ffprobe"),
            cjxl: locate("FORBIDDEN_LIBRARY_JXL_DIR", "jxl", "cjxl"),
            djxl: locate("FORBIDDEN_LIBRARY_JXL_DIR", "jxl", "djxl"),
        }
    }

    pub fn has_ffmpeg(&self) -> bool {
        self.ffmpeg.is_some()
    }

    /// Whether JPEG XL can be both written and read back. The optimizer
    /// never keeps a JPEG XL it has not been able to check.
    pub fn has_jxl(&self) -> bool {
        self.cjxl.is_some() && self.djxl.is_some()
    }

    pub fn ffmpeg(&self) -> Result<&Path> {
        self.ffmpeg
            .as_deref()
            .ok_or_else(|| Error::Msg("ffmpeg was not found".into()))
    }

    pub fn ffprobe(&self) -> Result<&Path> {
        self.ffprobe
            .as_deref()
            .ok_or_else(|| Error::Msg("ffprobe was not found".into()))
    }

    pub fn cjxl(&self) -> Result<&Path> {
        self.cjxl
            .as_deref()
            .ok_or_else(|| Error::Msg("cjxl was not found".into()))
    }

    pub fn djxl(&self) -> Result<&Path> {
        self.djxl
            .as_deref()
            .ok_or_else(|| Error::Msg("djxl was not found".into()))
    }
}

/// Find a bundled tool. An explicit directory from the environment wins, then
/// the folder the installer uses for the tool, then the folder beside the
/// app's executable, and finally `PATH`.
fn locate(env: &str, folder: &str, name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var(env) {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join(folder));
        dirs.push(parent.to_path_buf());
    }
    dirs.iter()
        .find_map(|dir| find_tool(dir, name))
        .or_else(|| path_tool(name))
}

fn find_tool(dir: &Path, name: &str) -> Option<PathBuf> {
    for candidate in [format!("{name}.exe"), name.to_string()] {
        let path = dir.join(candidate);
        if path.is_file() {
            return Some(path.canonicalize().unwrap_or(path));
        }
    }
    None
}

/// A bare executable on `PATH` is accepted only from an absolute directory:
/// a relative entry such as `.` would let whatever happens to sit in the
/// process's working directory stand in for FFmpeg.
fn path_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() {
            continue;
        }
        if let Some(found) = find_tool(&dir, name) {
            return Some(found);
        }
    }
    None
}

/// A command that never flashes a console window.
pub fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// What a bounded tool run produced.
pub struct Bounded {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// Run a media tool with a deadline, bounded output and, on Windows, a job
/// object around its process tree. The child never gets a console, stdin, or
/// the ability to outlive this call.
pub(crate) fn run_bounded(mut cmd: Command, deadline: Duration) -> std::io::Result<Bounded> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    let _containment = match job::attach(&child) {
        Ok(guard) => Some(guard),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let stdout = child.stdout.take().map(|pipe| drain(pipe, STDOUT_CAP));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, STDERR_CAP));
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = stderr
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    Ok(Bounded {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

/// Read a pipe to EOF, keeping at most `cap` bytes. The rest is consumed so
/// the child never blocks on a full pipe.
fn drain(mut pipe: impl Read + Send + 'static, cap: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..read.min(room)]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        kept
    })
}

/// Turn a failed run into the message the caller surfaces.
pub(crate) fn failure(what: &str, source: &Path, run: &Bounded) -> Error {
    if run.timed_out {
        return Error::Msg(format!(
            "{what} timed out after {}s for {}",
            DEADLINE.as_secs(),
            source.display()
        ));
    }
    Error::Msg(format!(
        "{what} failed for {}: {}",
        source.display(),
        String::from_utf8_lossy(&run.stderr).trim()
    ))
}

pub fn ffprobe_json(tools: &Tools, source: &Path) -> Option<serde_json::Value> {
    let probe = tools.ffprobe.as_deref()?;
    let mut cmd = command(probe);
    cmd.args([
        "-v",
        "error",
        "-max_alloc",
        MAX_ALLOC,
        "-threads",
        "1",
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        FORMATS,
        "-probesize",
        "8388608",
        "-analyzeduration",
        "5000000",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        "-show_stream_groups",
    ])
    .arg(source);
    let run = run_bounded(cmd, DEADLINE).ok()?;
    if !run.status.success() {
        return None;
    }
    serde_json::from_slice(&run.stdout).ok()
}

/// Input options shared by every decode: no network protocols, an allowlist
/// of media demuxers, one thread, and a bounded probe/allocation budget.
fn input_options(cmd: &mut Command) {
    bounded_input_options(cmd, "1", "1");
}

/// The same bounds as [`input_options`], with threads for real-time playback.
/// A player cannot wait on one decoding thread for a 1080p stream, but the
/// memory ceiling and the job object still limit what a bad file can cost.
fn playback_input_options(cmd: &mut Command) {
    bounded_input_options(cmd, "4", "2");
}

fn bounded_input_options(cmd: &mut Command, threads: &str, filter_threads: &str) {
    cmd.args([
        "-max_alloc",
        MAX_ALLOC,
        "-threads",
        threads,
        "-filter_threads",
        filter_threads,
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        FORMATS,
        "-probesize",
        "8388608",
        "-analyzeduration",
        "5000000",
    ]);
}

/// Refuse a generated file that is larger than `cap` bytes, and remove it so a
/// later read cannot mistake it for a good output.
fn check_output_size(dest: &Path, cap: u64) -> Result<()> {
    match std::fs::metadata(dest) {
        Ok(metadata) if metadata.len() <= cap => Ok(()),
        Ok(metadata) => {
            let _ = std::fs::remove_file(dest);
            Err(Error::Msg(format!(
                "generated preview is {} bytes, over the {} MiB limit",
                metadata.len(),
                cap / (1024 * 1024)
            )))
        }
        Err(error) => Err(Error::Msg(format!(
            "generated preview {} is missing: {error}",
            dest.display()
        ))),
    }
}

/// Extract one frame of a video as JPEG, scaled to fit `max_side`.
pub fn video_poster(
    tools: &Tools,
    source: &Path,
    dest: &Path,
    max_side: u32,
    at_ms: i64,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seconds = (at_ms.max(0) as f64 / 1000.0).to_string();
    let filter = format!(
        "scale=w='min(iw,{max_side})':h='min(ih,{max_side})':force_original_aspect_ratio=decrease"
    );
    let mut cmd = command(tools.ffmpeg()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    input_options(&mut cmd);
    cmd.args(["-ss", &seconds, "-i"]).arg(source).args([
        "-frames:v",
        "1",
        "-vf",
        &filter,
        "-sws_flags",
        SCALER,
        "-q:v",
        "2",
        "-y",
    ]);
    cmd.arg(dest);
    let run = run_bounded(cmd, DEADLINE)?;
    if !run.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(failure("ffmpeg poster", source, &run));
    }
    check_output_size(dest, OUTPUT_CAP)
}

/// Re-encode the audio of `source` into `dest` with `codec_args`. The metadata
/// comes from `sidecar` rather than the source, because Ogg cannot carry a
/// cover as a stream: the sidecar holds the tags and the cover as a picture
/// tag. The output is removed on failure.
pub fn transcode_audio(
    tools: &Tools,
    source: &Path,
    sidecar: &Path,
    dest: &Path,
    codec_args: &[String],
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = command(tools.ffmpeg()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    input_options(&mut cmd);
    cmd.arg("-i")
        .arg(source)
        .arg("-i")
        .arg(sidecar)
        .args(["-map", "0:a:0", "-map_metadata", "1"])
        .args(codec_args)
        .arg(dest);
    let run = run_bounded(cmd, TRANSCODE_DEADLINE)?;
    if !run.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(failure("audio encode", source, &run));
    }
    Ok(())
}

/// Copy the first picture stream of `source` to `dest` as it is stored, with
/// no re-encoding, so the cover's own bytes are preserved.
pub fn extract_cover(tools: &Tools, source: &Path, dest: &Path) -> Result<()> {
    let mut cmd = command(tools.ffmpeg()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    input_options(&mut cmd);
    cmd.arg("-i")
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-c:v",
            "copy",
            "-f",
            "image2",
        ])
        .arg(dest);
    let run = run_bounded(cmd, DEADLINE)?;
    if !run.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(failure("cover extraction", source, &run));
    }
    Ok(())
}

/// Decode one frame of an image format the Rust decoder does not support, to
/// a lossless PNG at `dest`. A PNG rather than a JPEG, because the caller
/// shows this frame as it is: a JPEG here would be a second lossy generation
/// on top of the one the format already carries.
pub fn image_poster(tools: &Tools, source: &Path, dest: &Path, max_side: u32) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let scale = format!(
        "scale=w='min(iw,{max_side})':h='min(ih,{max_side})':force_original_aspect_ratio=decrease"
    );
    let cap = still_output_cap(max_side);
    let run = image_poster_run(tools, source, dest, &["-vf", &scale])?;
    if run.status.success() {
        return check_output_size(dest, cap);
    }
    // A tiled HEIF/AVIF (what phones write) is a stream group that ffmpeg
    // stitches in a filter graph of its own, and `-vf` cannot be attached to
    // that. Scale the stitched group in the same graph instead.
    let _ = std::fs::remove_file(dest);
    let grid = format!("[0:g:0]{scale}[out]");
    let retry = image_poster_run(
        tools,
        source,
        dest,
        &["-filter_complex", &grid, "-map", "[out]"],
    )?;
    if retry.status.success() {
        return check_output_size(dest, cap);
    }
    let _ = std::fs::remove_file(dest);
    // The first attempt's diagnostics describe the ordinary case; the retry
    // only says the file has no stream group.
    Err(failure("ffmpeg image decode", source, &run))
}

/// The largest PNG a still of `max_side` may produce: one raw RGBA frame of
/// that size, plus headroom. The video cap is far too small for a full-size
/// lossless still, which is why stills have their own.
fn still_output_cap(max_side: u32) -> u64 {
    let side = u64::from(max_side);
    side * side * 4 + STILL_HEADROOM
}

fn image_poster_run(tools: &Tools, source: &Path, dest: &Path, filter: &[&str]) -> Result<Bounded> {
    let mut cmd = command(tools.ffmpeg()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    input_options(&mut cmd);
    // `-threads 1` here is the encoder's: left alone it starts a thread per
    // core, each with frame buffers, and a full-size still from a tiled HEIF
    // (dozens of tile decoders already running) then runs out of the job's
    // memory. One frame needs no encoder threads.
    cmd.arg("-i").arg(source).args(filter).args([
        "-sws_flags",
        SCALER,
        "-threads",
        "1",
        "-frames:v",
        "1",
        "-c:v",
        "png",
        // RGBA always: an opaque source still comes out with an opaque
        // alpha, and the caller drops it, so one format covers every input.
        "-pix_fmt",
        "rgba",
        // Fast compression: the PNG is a short-lived staging file, and speed
        // matters more than size on a full-resolution still.
        "-compression_level",
        "1",
        // Name the muxer explicitly, as with every staging output, so the
        // destination's extension never decides the format. PNG output goes
        // through the image2 muxer, which writes one file per frame.
        "-f",
        "image2",
        "-y",
    ]);
    cmd.arg(dest);
    Ok(run_bounded(cmd, DEADLINE)?)
}

/// A media tool whose stdout is a live stream. Dropping it kills the tool and
/// anything it started, so a player that stops reading (a seek, a closed
/// window) never leaves a decoder running behind it.
pub struct Stream {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
    #[cfg(windows)]
    _job: job::Containment,
}

impl Stream {
    /// The output pipe. It is taken rather than borrowed, so a reader thread
    /// can own it; later calls return `None`.
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// What the tool has written to stderr so far, trimmed. Only the first
    /// [`STDERR_CAP`] bytes are kept; the rest is drained and dropped.
    pub fn stderr_text(&self) -> String {
        let kept = self
            .stderr
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        String::from_utf8_lossy(&kept).trim().to_string()
    }

    /// The exit status once the tool has finished, without waiting for it.
    pub fn try_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // The tool may already have exited; neither call failing matters.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a long-lived tool with a live stdout. There is no deadline: a stream
/// lasts as long as the reader keeps reading, and dropping it ends the tool.
fn spawn_stream(mut cmd: Command) -> Result<Stream> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    let job = match job::attach(&child) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(pipe) = child.stderr.take() {
        // Diagnostics are only ever shown on failure, so they are kept in a
        // capped buffer. The pipe is still drained so the tool never blocks.
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || keep_capped(pipe, &sink));
    }
    Ok(Stream {
        stdout: child.stdout.take(),
        child,
        stderr,
        #[cfg(windows)]
        _job: job,
    })
}

/// Copy a pipe into `sink` until EOF, keeping at most [`STDERR_CAP`] bytes.
fn keep_capped(mut pipe: impl Read, sink: &Mutex<Vec<u8>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let mut kept = sink.lock().unwrap_or_else(|poison| poison.into_inner());
                let room = STDERR_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// What a decoded video stream should look like on its way out.
pub struct VideoOut {
    /// Exact frame size. The reader depends on every frame being
    /// `width * height * 4` bytes, so ffmpeg is told to scale to it.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Let ffmpeg pick a hardware decoder when one exists. Frames still
    /// arrive as RGBA in system memory.
    pub hwaccel: bool,
    /// Seek by decoding from the start and discarding up to `start_ms`,
    /// instead of jumping in the demuxer. Much slower, but it is the only
    /// seek that works in a stream with no index and few keyframes (a long
    /// GOP MPEG-TS), where a demuxer jump lands after the last keyframe the
    /// rest of the file needs.
    pub exact_seek: bool,
}

/// Decode the first video stream of `source` from `start_ms`, delivering RGBA
/// frames of `out.width` x `out.height` at a constant `out.fps`. Autorotate
/// stays on, so `out` must describe the rotated (display) size.
pub fn spawn_video(tools: &Tools, source: &Path, start_ms: i64, out: &VideoOut) -> Result<Stream> {
    if out.width == 0 || out.height == 0 || !(out.fps.is_finite() && out.fps > 0.0) {
        return Err(Error::Msg(
            "video output needs a non-zero size and a positive frame rate".into(),
        ));
    }
    let mut cmd = command(tools.ffmpeg()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if out.hwaccel {
        cmd.args(["-hwaccel", "auto"]);
    }
    playback_input_options(&mut cmd);
    if out.exact_seek {
        cmd.arg("-i").arg(source);
        seek_args(&mut cmd, start_ms);
    } else {
        seek_args(&mut cmd, start_ms);
        cmd.arg("-i").arg(source);
    }
    let filter = format!(
        "scale={}:{}:flags=bilinear,setsar=1,format=rgba",
        out.width, out.height
    );
    cmd.args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf"])
        .arg(filter)
        .args(["-fps_mode", "cfr", "-r"])
        .arg(frame_rate_arg(out.fps))
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
    spawn_stream(cmd)
}

/// Decode the first audio stream of `source` from `start_ms` as interleaved
/// 32-bit float PCM at `sample_rate` and `channels`.
pub fn spawn_audio(
    tools: &Tools,
    source: &Path,
    start_ms: i64,
    sample_rate: u32,
    channels: u16,
) -> Result<Stream> {
    if sample_rate == 0 || channels == 0 {
        return Err(Error::Msg(
            "audio output needs a sample rate and a channel count".into(),
        ));
    }
    let mut cmd = command(tools.ffmpeg()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    playback_input_options(&mut cmd);
    seek_args(&mut cmd, start_ms);
    cmd.arg("-i")
        .arg(source)
        .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac"])
        .arg(channels.to_string())
        .arg("-ar")
        .arg(sample_rate.to_string())
        .args(["-f", "f32le", "pipe:1"]);
    spawn_stream(cmd)
}

/// `-ss` before `-i`, so ffmpeg seeks in the demuxer instead of decoding from
/// the start. Zero or negative means the beginning and adds no argument.
fn seek_args(cmd: &mut Command, start_ms: i64) {
    if start_ms > 0 {
        cmd.args(["-ss", &format!("{:.3}", start_ms as f64 / 1000.0)]);
    }
}

/// A frame rate in the form `-r` takes: up to six decimals, no trailing zeros.
fn frame_rate_arg(fps: f64) -> String {
    let text = format!("{fps:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Windows job-object containment for a media tool and anything it starts.
///
/// The limits are resource containment, not a privilege sandbox: the child
/// still runs with the user's file permissions. They exist so one malformed
/// file cannot exhaust memory or leave a decoder process behind after the
/// app closes.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// 1 GiB combined for the tool and its children.
    const MEMORY_LIMIT: usize = 1024 * 1024 * 1024;
    /// FFmpeg sometimes starts one helper; three is slack, not a budget.
    const PROCESS_LIMIT: u32 = 3;

    pub struct Containment(HANDLE);

    // SAFETY: the handle is a kernel object with no thread affinity. The guard
    // is only ever dropped, never used from two threads at once, so a stream
    // may be handed to another thread along with its containment.
    unsafe impl Send for Containment {}

    pub fn attach(child: &Child) -> std::io::Result<Containment> {
        // SAFETY: the handles are owned by this guard and closed once, and
        // the limits structure is initialized before it is read.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let guard = Containment(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_JOB_MEMORY
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = PROCESS_LIMIT;
            limits.JobMemoryLimit = MEMORY_LIMIT;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if AssignProcessToJobObject(handle, child.as_raw_handle().cast()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(guard)
        }
    }

    impl Drop for Containment {
        fn drop(&mut self) {
            // SAFETY: the handle was created by CreateJobObjectW and is owned
            // by this object; closing it also kills the remaining tree.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lossless_still_may_be_as_large_as_its_raw_pixels() {
        let full = u64::from(FULL_STILL_SIDE);
        assert!(still_output_cap(FULL_STILL_SIDE) >= full * full * 4);
        assert!(still_output_cap(FULL_STILL_SIDE) < full * full * 4 + STILL_HEADROOM + 1);
    }

    #[test]
    fn jpeg_xl_needs_both_of_its_tools() {
        let tools = Tools {
            ffmpeg: None,
            ffprobe: None,
            cjxl: Some(PathBuf::from("cjxl.exe")),
            djxl: None,
        };
        assert!(!tools.has_jxl());
        let both = Tools {
            djxl: Some(PathBuf::from("djxl.exe")),
            ..tools
        };
        assert!(both.has_jxl());
    }

    #[test]
    fn a_bundled_tool_is_found_by_name_in_its_folder() {
        let temp = tempfile::tempdir().unwrap();
        assert!(find_tool(temp.path(), "cjxl").is_none());
        std::fs::write(temp.path().join("cjxl.exe"), b"encoder").unwrap();
        let found = find_tool(temp.path(), "cjxl").unwrap();
        assert_eq!(found.file_name().unwrap(), "cjxl.exe");
    }

    const FULL_STILL_SIDE: u32 = 8192;
}
