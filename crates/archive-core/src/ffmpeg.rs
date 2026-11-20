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
//! also placed in a job object with a memory and process-count limit; on Unix
//! it becomes a process-group leader under rlimits on memory, core dumps and
//! file size, so killing the call kills everything the tool started. Either
//! way a decoder that misbehaves cannot take the whole app with it, and cannot
//! outlive the call.

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
/// On Linux, the anonymous memory a tool process may map. It is far above a
/// real decode or transcode (FFmpeg's own ceiling is [`MAX_ALLOC`]) and exists
/// so a decoder that runs away hits a wall instead of the machine's memory.
#[cfg(target_os = "linux")]
const TOOL_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
/// On Linux, the largest file a tool may write. A single transcode is never
/// near it; a muxer looping over its input would be, and fills the disk.
#[cfg(target_os = "linux")]
const TOOL_FILE: u64 = 4 * 1024 * 1024 * 1024;
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

/// The file name a bundled tool has. The Windows installer ships `ffmpeg.exe`
/// and `cjxl.exe`; a Unix build ships the tools under their plain names, so
/// searching for the Windows spelling there would find nothing.
#[cfg(windows)]
const TOOL_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const TOOL_SUFFIX: &str = "";

fn find_tool(dir: &Path, name: &str) -> Option<PathBuf> {
    // Windows takes an `.exe` first and then an extensionless copy, so a
    // hand-placed build keeps working; Unix only ever looks for the plain
    // name, because an `.exe` there is some unrelated file.
    #[cfg(windows)]
    let candidates = [format!("{name}{TOOL_SUFFIX}"), name.to_string()];
    #[cfg(not(windows))]
    let candidates = [format!("{name}{TOOL_SUFFIX}")];
    candidates.iter().find_map(|candidate| {
        let path = dir.join(candidate);
        executable(&path).then(|| path.canonicalize().unwrap_or(path))
    })
}

/// Whether `path` is a file this platform can run. On Unix the execute bit is
/// what makes a file a program: a stray document named `ffmpeg` beside the app
/// or on `PATH` must not be mistaken for one.
fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
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
    // Only Windows changes the command here; Unix containment is applied by
    // the spawn sites, which have to reason about when the child is reaped.
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Put a tool in its own process group, and on Linux under the resource
/// ceilings a decoder must not cross.
///
/// On Unix the process group is what makes the child tree killable: a signal
/// to the negated group reaches FFmpeg and every helper it started, which a
/// signal to the child alone would leave running. The rlimits are backstops,
/// not budgets: a real transcode stays far below them, so only a decoder that
/// has run away ever hits one. They are set between fork and exec, where only
/// async-signal-safe calls are allowed, and a `setrlimit` failure is ignored so
/// a host with a lower hard limit still runs the tool.
#[cfg(unix)]
fn contain(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    // SAFETY: the closure allocates nothing and only calls getrlimit and
    // setrlimit, both of which are async-signal-safe.
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            cap(libc::RLIMIT_CORE, 0);
            cap(libc::RLIMIT_DATA, TOOL_MEMORY);
            cap(libc::RLIMIT_FSIZE, TOOL_FILE);
            Ok(())
        });
    }
}

/// Lower one rlimit's soft limit, never raising it. Failure is not an error:
/// the containment is a backstop, and a host that forbids the limit must still
/// be able to run the tool.
#[cfg(target_os = "linux")]
fn cap(resource: libc::__rlimit_resource_t, value: u64) {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both calls only read and write this process's own limits, and
    // `limits` is initialized before it is passed.
    unsafe {
        if libc::getrlimit(resource, &mut limits) != 0 {
            return;
        }
        if limits.rlim_cur != libc::RLIM_INFINITY && limits.rlim_cur <= value {
            return;
        }
        limits.rlim_cur = value.min(limits.rlim_max);
        libc::setrlimit(resource, &limits);
    }
}

/// What a bounded tool run produced.
pub struct Bounded {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// Run a media tool with a deadline, bounded output and a job object (Windows)
/// or process group (Unix) around its process tree. The child never gets a
/// console, stdin, or the ability to outlive this call.
pub(crate) fn run_bounded(mut cmd: Command, deadline: Duration) -> std::io::Result<Bounded> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    contain(&mut cmd);
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
    #[cfg(unix)]
    let mut containment = group::Containment::attach(&child);
    let stdout = child.stdout.take().map(|pipe| drain(pipe, STDOUT_CAP));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, STDERR_CAP));
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            // Waiting reaps the child, which frees its process-group id for
            // the kernel to hand to something else. The tool finished on its
            // own and closed its pipes, so containment ends here.
            #[cfg(unix)]
            containment.disarm();
            break status;
        }
        if started.elapsed() >= deadline {
            timed_out = true;
            #[cfg(unix)]
            containment.kill();
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
    #[cfg(unix)]
    group: group::Containment,
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
        let status = self.child.try_wait().ok().flatten();
        #[cfg(unix)]
        if status.is_some() {
            // Waiting reaps the tool and frees its process-group id for reuse,
            // so the group must not be signalled after this point.
            self.group.disarm();
        }
        status
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // The group goes first: the tool is still unreaped, so its group id
        // cannot have been recycled, and anything it started dies with it.
        #[cfg(unix)]
        self.group.kill();
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
    #[cfg(unix)]
    contain(&mut cmd);
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
    #[cfg(unix)]
    let group = group::Containment::attach(&child);
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
        #[cfg(unix)]
        group,
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

/// Unix process-group containment for a media tool and anything it starts.
///
/// The tool is spawned as its own process-group leader (see [`contain`]), so
/// one signal to the negated group reaches every process it started. The group
/// is signalled only while the direct child is still unreaped: waiting for it
/// frees the group id for the kernel to reuse, and a signal to a recycled id
/// could hit an unrelated process. A tool that exits on its own has closed its
/// pipes by then, which is what makes the wait safe.
#[cfg(unix)]
mod group {
    use std::process::Child;

    pub struct Containment {
        pgid: libc::pid_t,
        armed: bool,
    }

    impl Containment {
        /// Track the child's process group. [`contain`] made the child a
        /// group leader, so its own pid is the group id.
        pub fn attach(child: &Child) -> Self {
            Self {
                pgid: child.id() as libc::pid_t,
                armed: true,
            }
        }

        /// SIGKILL the whole group. See the module comment for when this is
        /// safe to call.
        pub fn kill(&mut self) {
            if !self.armed {
                return;
            }
            self.armed = false;
            // SAFETY: a negated pid means "this process group", and SIGKILL
            // needs no handler. A group that has already emptied gives ESRCH,
            // which is the outcome wanted anyway.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }

        /// Stop tracking the group because the child has been reaped.
        pub fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for Containment {
        fn drop(&mut self) {
            self.kill();
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

    /// Write a file `find_tool` should accept: the platform's own name
    /// spelling, and on Unix an execute bit.
    fn write_tool(dir: &Path, name: &str) {
        let path = dir.join(format!("{name}{TOOL_SUFFIX}"));
        std::fs::write(&path, b"tool").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn a_bundled_tool_is_found_by_name_in_its_folder() {
        let temp = tempfile::tempdir().unwrap();
        assert!(find_tool(temp.path(), "cjxl").is_none());
        write_tool(temp.path(), "cjxl");
        let found = find_tool(temp.path(), "cjxl").unwrap();
        assert_eq!(
            found.file_name().unwrap(),
            format!("cjxl{TOOL_SUFFIX}").as_str()
        );
    }

    /// The Windows spelling must not be picked up on Unix, where a bundled
    /// tool has no extension and `cjxl.exe` would be some unrelated file.
    #[cfg(unix)]
    #[test]
    fn a_tool_must_be_executable_and_use_the_platform_name() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cjxl");
        std::fs::write(&path, b"encoder").unwrap();
        assert!(
            find_tool(temp.path(), "cjxl").is_none(),
            "a file without an execute bit is not a tool"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_tool(temp.path(), "cjxl").unwrap(),
            path.canonicalize().unwrap()
        );

        let windows_spelling = temp.path().join("djxl.exe");
        std::fs::write(&windows_spelling, b"encoder").unwrap();
        std::fs::set_permissions(&windows_spelling, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(find_tool(temp.path(), "djxl").is_none());
    }

    /// A tool that forks must not outlive the call that started it. `sh`
    /// starts a background sleep and then a foreground one, so killing only
    /// the direct child would leave the background one running for minutes.
    #[cfg(unix)]
    #[test]
    fn a_deadline_kills_the_tools_whole_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("pgid");
        let mut cmd = command(Path::new("sh"));
        cmd.arg("-c").arg(format!(
            "echo $$ > '{}'; sleep 300 & sleep 300",
            pid_file.display()
        ));
        let run = run_bounded(cmd, Duration::from_millis(500)).unwrap();
        assert!(run.timed_out, "the tool should have hit its deadline");
        let pgid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        // `run_bounded` makes the tool a group leader, so the shell's own pid
        // (what `$$` reports) is the group id. SIGKILL reached the whole group;
        // the reaping of the orphaned sleeps happens outside this process.
        let group_is_gone = || {
            // SAFETY: signal 0 only asks whether the group still has members.
            unsafe { libc::kill(-pgid, 0) == -1 }
        };
        for _ in 0..200 {
            if group_is_gone() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process group {pgid} outlived the bounded run");
    }

    const FULL_STILL_SIDE: u32 = 8192;
}
