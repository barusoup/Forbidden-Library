//! Semantic search, entirely in the background.
//!
//! The app ships EmbeddingGemma 2 (GGUF) and a llama.cpp runtime. On first
//! use a private `llama-server` is started, preferring the Vulkan build (so
//! the GPU is used when one is available) and falling back to the CPU build.
//! Every imported preview is embedded once and stored in the catalogue; a
//! typed query is embedded into the same space and ranked by cosine
//! similarity. None of this is surfaced in the UI, and the server is shut
//! down with the app.

use crate::worker::AppEvent;
use archive_core::{EMBED_DIMS, EMBED_MODEL, Item, Library, VectorIndex, prepare};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use winit::event_loop::EventLoopProxy;

/// The model card's retrieval prefix. Images are embedded without it.
const QUERY_PREFIX: &str = "task: search result | query: ";
const TOP_RESULTS: usize = 300;
/// Similarity above a neutral baseline. Raw cosine is nearly useless here:
/// this model scores unrelated media around 0.65 for almost any text. A hit
/// has to be at least this much closer to the query than the blank prefix is.
const MIN_DELTA: f32 = 0.02;
/// Images embedded per request. One round trip carries several previews, so
/// request, base64 and response parsing overhead is paid once instead of per
/// photo; the model itself still works through them in order.
const BATCH: usize = 8;
/// Query vectors are reused while the user edits a query, which turns every
/// backspace-and-retype into a pure index scan.
const QUERY_CACHE: usize = 8;
const SCAN_EVERY: Duration = Duration::from_secs(5);
const MODEL_FILE: &str = "model/embeddinggemma-2-UD-Q4_K_XL.gguf";
const MMPROJ_FILE: &str = "model/mmproj-Q8_0.gguf";

enum Job {
    Wake,
    Query { seq: u64, text: String },
    Shutdown,
}

/// Handle the UI uses to wake the indexer and issue queries. Dropping it
/// stops the worker and the inference server.
pub struct Semantic {
    sender: Sender<Job>,
    thread: Option<JoinHandle<()>>,
}

impl Semantic {
    /// `None` when the bundled model or runtime is missing; the app then
    /// simply runs without semantic matches instead of failing.
    pub fn start(
        library: Arc<Library>,
        events: Sender<AppEvent>,
        proxy: Option<EventLoopProxy<()>>,
    ) -> Option<Self> {
        let assets = Assets::discover(library.paths().logs())?;
        let (sender, receiver) = channel();
        let thread = std::thread::Builder::new()
            .name("forbiddenlibrary-search".into())
            .spawn(move || run(assets, library, events, proxy, receiver))
            .ok()?;
        Some(Self {
            sender,
            thread: Some(thread),
        })
    }

    /// New media (or a new import) is waiting to be indexed.
    pub fn wake(&self) {
        let _ = self.sender.send(Job::Wake);
    }

    /// Embed `text` and send the ranked ids back as `AppEvent::SemanticResults`.
    pub fn query(&self, seq: u64, text: String) {
        let _ = self.sender.send(Job::Query { seq, text });
    }
}

impl Drop for Semantic {
    fn drop(&mut self) {
        let _ = self.sender.send(Job::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Runtime {
    exe: PathBuf,
    gpu: bool,
}

struct Assets {
    model: PathBuf,
    mmproj: PathBuf,
    runtimes: Vec<Runtime>,
    log: PathBuf,
}

impl Assets {
    fn discover(logs: &Path) -> Option<Self> {
        let mut bases: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::var_os("FORBIDDEN_LIBRARY_SEMANTIC_DIR") {
            bases.push(PathBuf::from(dir));
        }
        // Installed: `<app>/semantic`. Development: the workspace's
        // `vendor/semantic`, reached from either the binary or the test
        // harness deep under `target/`.
        let mut visit = |start: Option<&Path>| {
            let mut current = start.map(Path::to_path_buf);
            for _ in 0..6 {
                let Some(dir) = current else { break };
                bases.push(dir.join("semantic"));
                bases.push(dir.join("vendor/semantic"));
                current = dir.parent().map(Path::to_path_buf);
            }
        };
        visit(
            std::env::current_exe()
                .ok()
                .as_deref()
                .and_then(Path::parent),
        );
        visit(std::env::current_dir().ok().as_deref());
        for base in bases {
            let model = base.join(MODEL_FILE);
            let mmproj = base.join(MMPROJ_FILE);
            if !model.is_file() || !mmproj.is_file() {
                continue;
            }
            let mut runtimes = Vec::new();
            for (folder, gpu) in [("runtime/vulkan", true), ("runtime/cpu", false)] {
                let exe = base.join(folder).join("llama-server.exe");
                if exe.is_file() {
                    runtimes.push(Runtime { exe, gpu });
                }
            }
            if runtimes.is_empty() {
                continue;
            }
            return Some(Self {
                model,
                mmproj,
                runtimes,
                log: logs.join("search.log"),
            });
        }
        None
    }
}

struct Server {
    child: Child,
    port: u16,
    key: String,
    agent: ureq::Agent,
    job: Option<isize>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job as *mut _);
            }
        }
    }
}

/// Put the inference process in a kill-on-close job, so it cannot outlive the
/// app even if the app is terminated without running destructors.
#[cfg(windows)]
fn attach_job(child: &Child) -> Option<isize> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        let assigned = AssignProcessToJobObject(job, child.as_raw_handle() as *mut _);
        if configured != 0 && assigned != 0 {
            Some(job as isize)
        } else {
            windows_sys::Win32::Foundation::CloseHandle(job);
            None
        }
    }
}

#[cfg(not(windows))]
fn attach_job(_child: &Child) -> Option<isize> {
    None
}

impl Server {
    fn start(assets: &Assets) -> Option<Self> {
        for runtime in &assets.runtimes {
            match Self::launch(assets, runtime) {
                Ok(server) => {
                    log(
                        &assets.log,
                        &format!("started {} on port {}", runtime.exe.display(), server.port),
                    );
                    return Some(server);
                }
                Err(error) => log(
                    &assets.log,
                    &format!("{} could not start: {error}", runtime.exe.display()),
                ),
            }
        }
        None
    }

    fn launch(assets: &Assets, runtime: &Runtime) -> Result<Self, String> {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("no local port: {error}"))?
            .local_addr()
            .map_err(|error| format!("no local port: {error}"))?
            .port();
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|error| format!("API key randomness: {error}"))?;
        let key: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&assets.log)
            .map_err(|error| format!("open log: {error}"))?;
        let err = out
            .try_clone()
            .map_err(|error| format!("open log: {error}"))?;
        let mut command = Command::new(&runtime.exe);
        command
            .arg("--model")
            .arg(&assets.model)
            .arg("--mmproj")
            .arg(&assets.mmproj)
            .arg("--embeddings")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--api-key")
            .arg(&key)
            .arg("--ctx-size")
            .arg("4096")
            .arg("--batch-size")
            .arg("4096")
            .arg("--ubatch-size")
            .arg("4096")
            .arg("--parallel")
            .arg("1")
            .arg("-ngl")
            .arg(if runtime.gpu { "99" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|error| format!("spawn: {error}"))?;
        let job = attach_job(&child);
        let agent: ureq::Agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .timeout_write(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(60))
            .build();
        let health = format!("http://127.0.0.1:{port}/health");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                let _ = child.kill();
                return Err(format!("exited during startup ({status})"));
            }
            if let Ok(response) = agent.get(&health).call()
                && response.status() == 200
            {
                return Ok(Self {
                    child,
                    port,
                    key,
                    agent,
                    job,
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err("timed out while loading the model".into())
    }

    fn embed(&self, input: Value) -> Result<Vec<Vec<f32>>, String> {
        let url = format!("http://127.0.0.1:{}/v1/embeddings", self.port);
        let body = json!({ "model": "embeddinggemma-2", "input": input });
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.key))
            .send_json(body)
            .map_err(|error| format!("embedding request: {error}"))?;
        let parsed: EmbeddingResponse = response
            .into_json()
            .map_err(|error| format!("embedding response: {error}"))?;
        let count = parsed.data.len();
        let mut vectors = vec![Vec::new(); count];
        for (position, datum) in parsed.data.into_iter().enumerate() {
            let index = datum.index.unwrap_or(position);
            if index < vectors.len() {
                vectors[index] = datum.embedding;
            }
        }
        if vectors.is_empty() || vectors.iter().any(|vector| vector.is_empty()) {
            return Err("embedding response was incomplete".into());
        }
        Ok(vectors)
    }

    fn embed_prefixed(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut vectors = self.embed(json!(text))?;
        vectors.pop().ok_or_else(|| "no query vector".into())
    }

    fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
        self.embed_prefixed(&format!("{QUERY_PREFIX}{text}"))
    }

    /// The embedding of the instruction prefix alone. Every query is scored
    /// relative to it; computed once per server.
    fn embed_baseline(&self) -> Result<Vec<f32>, String> {
        self.embed_prefixed(QUERY_PREFIX)
    }

    fn embed_images(&self, frames: &[Vec<u8>]) -> Result<Vec<Vec<f32>>, String> {
        let mut input: Vec<Value> = Vec::with_capacity(frames.len());
        for frame in frames {
            let mut data = String::with_capacity(frame.len() / 3 * 4 + 4);
            data.push_str("data:image/jpeg;base64,");
            STANDARD.encode_string(frame, &mut data);
            input.push(json!({
                "content": [{
                    "type": "image_url",
                    "image_url": { "url": data }
                }]
            }));
        }
        self.embed(Value::Array(input))
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    embedding: Vec<f32>,
}

/// Everything one worker needs to answer queries: the lazily started
/// server, the baseline vector and the recent query vectors.
struct Searcher {
    server: Option<Server>,
    baseline: Option<Vec<f32>>,
    unavailable: bool,
    cache: VecDeque<(String, Vec<f32>)>,
}

impl Searcher {
    fn new() -> Self {
        Self {
            server: None,
            baseline: None,
            unavailable: false,
            cache: VecDeque::with_capacity(QUERY_CACHE),
        }
    }

    /// Best-first ids that are meaningfully closer to the query than to the
    /// blank instruction prefix. That delta, not raw cosine, is what
    /// separates a real match from the model's large shared similarity.
    fn answer(&mut self, text: &str, index: &VectorIndex, assets: &Assets) -> Vec<String> {
        let text = text.trim();
        // A single letter carries no reliable meaning and scores like noise.
        if index.is_empty() || text.chars().count() < 2 {
            return Vec::new();
        }
        let Some(query) = self.query_vector(text, assets) else {
            return Vec::new();
        };
        let Some(baseline) = self.baseline(assets) else {
            return Vec::new();
        };
        index
            .search_relative(&query, &baseline, TOP_RESULTS, MIN_DELTA)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn query_vector(&mut self, text: &str, assets: &Assets) -> Option<Vec<f32>> {
        if let Some(position) = self.cache.iter().position(|(cached, _)| cached == text) {
            let cached = self.cache.remove(position)?;
            let vector = cached.1.clone();
            self.cache.push_front(cached);
            return Some(vector);
        }
        let server = self.server(assets)?;
        let vector = match server.embed_text(text) {
            Ok(vector) => prepare(&vector, EMBED_DIMS),
            Err(error) => {
                log(&assets.log, &format!("query failed: {error}"));
                return None;
            }
        };
        if vector.is_empty() {
            return None;
        }
        self.cache.push_front((text.to_string(), vector.clone()));
        self.cache.truncate(QUERY_CACHE);
        Some(vector)
    }

    fn baseline(&mut self, assets: &Assets) -> Option<Vec<f32>> {
        if let Some(baseline) = &self.baseline {
            return Some(baseline.clone());
        }
        let server = self.server(assets)?;
        match server.embed_baseline() {
            Ok(vector) => {
                let prepared = prepare(&vector, EMBED_DIMS);
                if prepared.is_empty() {
                    return None;
                }
                self.baseline = Some(prepared.clone());
                Some(prepared)
            }
            Err(error) => {
                log(&assets.log, &format!("baseline failed: {error}"));
                None
            }
        }
    }

    fn server(&mut self, assets: &Assets) -> Option<&mut Server> {
        if self.server.is_none() && !self.unavailable {
            match Server::start(assets) {
                Some(server) => self.server = Some(server),
                None => {
                    self.unavailable = true;
                    log(&assets.log, "no runtime could be started");
                }
            }
        }
        self.server.as_mut()
    }
}

fn run(
    assets: Assets,
    library: Arc<Library>,
    events: Sender<AppEvent>,
    proxy: Option<EventLoopProxy<()>>,
    commands: Receiver<Job>,
) {
    // Load the stored vectors straight into the index's layout: with a large
    // library this is one decode pass and one buffer instead of a small
    // allocation and a copy per photo.
    let (ids, vectors) = library
        .flat_embeddings(EMBED_MODEL, EMBED_DIMS)
        .unwrap_or_default();
    let mut index = VectorIndex::from_flat(EMBED_DIMS, ids, vectors);
    let mut queue: VecDeque<Item> = VecDeque::new();
    let mut next_scan = Instant::now();
    let mut search = Searcher::new();
    let mut last_notice = Instant::now() - Duration::from_secs(2);

    loop {
        match commands.try_recv() {
            Ok(Job::Shutdown) | Err(TryRecvError::Disconnected) => break,
            Ok(Job::Wake) => {
                queue.clear();
                next_scan = Instant::now();
            }
            Ok(Job::Query { seq, text }) => {
                let ids = search.answer(&text, &index, &assets);
                let _ = events.send(AppEvent::SemanticResults { seq, ids });
                wake(&proxy);
            }
            Err(TryRecvError::Empty) => {}
        }

        // Only look for more work once the current batch is drained, so the
        // same item is never queued twice while a slow CPU works through it.
        if queue.is_empty() && next_scan <= Instant::now() {
            match library.pending_embeddings(EMBED_MODEL, 64) {
                Ok(items) => queue.extend(items),
                Err(error) => log(&assets.log, &format!("scan failed: {error}")),
            }
            next_scan = Instant::now() + SCAN_EVERY;
        }

        if queue.is_empty() {
            match commands.recv_timeout(Duration::from_millis(250)) {
                Ok(Job::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Job::Wake) => {
                    queue.clear();
                    next_scan = Instant::now();
                }
                Ok(Job::Query { seq, text }) => {
                    let ids = search.answer(&text, &index, &assets);
                    let _ = events.send(AppEvent::SemanticResults { seq, ids });
                    wake(&proxy);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }

        if search.unavailable {
            // No runtime could be started. Leave the items pending for a
            // later launch rather than reading previews for nothing.
            queue.clear();
            match commands.recv_timeout(Duration::from_secs(2)) {
                Ok(Job::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Job::Wake) => next_scan = Instant::now(),
                Ok(Job::Query { seq, text }) => {
                    let ids = search.answer(&text, &index, &assets);
                    let _ = events.send(AppEvent::SemanticResults { seq, ids });
                    wake(&proxy);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }

        let batch: Vec<Item> = queue.drain(..queue.len().min(BATCH)).collect();
        if !index_batch(&batch, &library, &mut index, &mut search, &assets) {
            // The server failed mid-batch. Drop the batch; the next scan
            // retries, and a failed start disables indexing for this run.
            queue.clear();
            next_scan = Instant::now() + Duration::from_secs(10);
            let _ = commands.recv_timeout(Duration::from_millis(500));
            continue;
        }
        if last_notice.elapsed() >= Duration::from_secs(1) {
            last_notice = Instant::now();
            let _ = events.send(AppEvent::SemanticAdvanced);
            wake(&proxy);
        }
        if queue.is_empty() {
            // Keep going immediately if more media is already waiting.
            next_scan = Instant::now();
            let _ = events.send(AppEvent::SemanticAdvanced);
            wake(&proxy);
        }
    }

    log(&assets.log, "indexer stopped");
}

/// Embed one batch and commit it. Returns false when the server is unusable,
/// so the caller stops hammering it.
fn index_batch(
    batch: &[Item],
    library: &Library,
    index: &mut VectorIndex,
    search: &mut Searcher,
    assets: &Assets,
) -> bool {
    let mut frames = Vec::new();
    let mut queued = Vec::new();
    let mut rows: Vec<(String, Vec<f32>)> = Vec::new();
    for item in batch {
        match library.embedding_frame(item) {
            Ok(frame) if !frame.is_empty() => {
                frames.push(frame);
                queued.push(item);
            }
            Ok(_) => rows.push((item.id.clone(), vec![0.0; EMBED_DIMS])),
            Err(error) => {
                log(
                    &assets.log,
                    &format!("{} has no usable preview: {error}", item.id),
                );
                // A zero vector records the attempt, so the item is not
                // retried forever; it can never pass the search cutoff.
                rows.push((item.id.clone(), vec![0.0; EMBED_DIMS]));
            }
        }
    }
    if !frames.is_empty() {
        let Some(server) = search.server(assets) else {
            return false;
        };
        let vectors = match server.embed_images(&frames) {
            Ok(vectors) => vectors,
            Err(error) => {
                log(&assets.log, &format!("indexing failed: {error}"));
                return false;
            }
        };
        for (item, vector) in queued.into_iter().zip(vectors) {
            let prepared = prepare(&vector, EMBED_DIMS);
            if prepared.is_empty() {
                rows.push((item.id.clone(), vec![0.0; EMBED_DIMS]));
            } else {
                rows.push((item.id.clone(), prepared));
            }
        }
    }
    if rows.is_empty() {
        return true;
    }
    if let Err(error) = library.set_embeddings(EMBED_MODEL, &rows) {
        log(&assets.log, &format!("store vectors: {error}"));
        return true;
    }
    for (id, vector) in rows {
        index.insert(id, vector);
    }
    true
}

fn wake(proxy: &Option<EventLoopProxy<()>>) {
    if let Some(proxy) = proxy {
        let _ = proxy.send_event(());
    }
}

fn log(path: &Path, message: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_core::{ImportOptions, import_paths};
    use std::sync::mpsc;

    /// The real thing: bundled GGUF + runtime, import, background indexing,
    /// then a text query. Ignored by default because it loads the model.
    #[test]
    #[ignore = "loads the bundled model and needs a GPU or fast CPU"]
    fn bundled_model_ranks_images_by_text() {
        let temp = tempfile::TempDir::new().unwrap();
        let media = temp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let colors = [
            ("red.png", [220u8, 30, 30]),
            ("blue.png", [30, 30, 220]),
            ("green.png", [30, 190, 70]),
        ];
        for (name, color) in colors {
            let (width, height) = (384u32, 384u32);
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[0] = color[0];
                pixel[1] = color[1];
                pixel[2] = color[2];
                pixel[3] = 255;
            }
            let path = media.join(name);
            std::fs::write(&path, dream::png::encode(width, height, &pixels)).unwrap();
        }

        let library = Arc::new(Library::open(temp.path().join("library")).unwrap());
        let report = import_paths(&library, &[media], &ImportOptions::default(), &mut |_| {});
        assert_eq!(report.imported, 3, "{report:?}");

        let (events, received) = mpsc::channel();
        let Some(semantic) = Semantic::start(Arc::clone(&library), events, None) else {
            panic!("the bundled model or runtime is missing");
        };
        let deadline = Instant::now() + Duration::from_secs(240);
        while library.embedding_count(EMBED_MODEL).unwrap() < 3 {
            assert!(Instant::now() < deadline, "indexing timed out");
            std::thread::sleep(Duration::from_millis(200));
        }

        let items = library
            .items(&archive_core::TimelineFilter::default())
            .unwrap();
        let red = items
            .iter()
            .find(|item| item.original_name == "red.png")
            .unwrap();
        let blue = items
            .iter()
            .find(|item| item.original_name == "blue.png")
            .unwrap();
        semantic.query(7, "a solid red square".into());
        let ids = loop {
            match received.recv_timeout(Duration::from_secs(90)) {
                Ok(AppEvent::SemanticResults { seq: 7, ids }) => break ids,
                Ok(_) => {}
                Err(error) => panic!("no search results: {error}"),
            }
        };
        assert_eq!(ids.first().map(String::as_str), Some(red.id.as_str()));
        assert!(!ids.contains(&blue.id), "blue square passed the red query");
        drop(semantic);
    }
}
