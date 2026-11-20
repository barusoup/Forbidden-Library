//! Forbidden Library — the app.
//!
//! The window, renderer, UI and frame loop belong to Dream's app runner
//! ([`dream::ui::app::run`]); the app in [`app`] fills the frames it asks for
//! and keeps all storage work on background threads, waking the runner when
//! results land.
//!
//! ```text
//! forbidden-library                 # the app
//! forbidden-library --selfcheck     # headless engine + UI check
//! forbidden-library --rendercheck   # headless render of every page and the viewer
//! forbidden-library --backup <dir>  # consistent copy of the whole library
//! ```

#![cfg_attr(
    all(target_os = "windows", not(feature = "console")),
    windows_subsystem = "windows"
)]

mod app;
mod autolock;
mod details;
mod fonts;
mod grid;
mod icons;
mod media;
mod motion;
mod remote_import;
mod semantic;
mod settings;
mod theme;
mod thumbs;
mod updater;
mod viewer;
mod worker;

use app::ArchiveApp;
use dream::ui::app::{AppContext, AppOptions, UiApp, run};
use dream::ui::widgets::Ui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Theme, Window};

/// The shipped app: the Dream runner owns the window, renderer, UI and the
/// scheduling; this holds the archive on top of it.
struct LibraryApp {
    app: Option<ArchiveApp>,
    /// Wakes the runner for the app's own timers, which the runner cannot see.
    deadline: Option<Deadline>,
    /// The OS appearance last applied, so a theme change is noticed once.
    last_dark: Option<bool>,
}

impl LibraryApp {
    fn new() -> Self {
        Self {
            app: None,
            deadline: None,
            last_dark: None,
        }
    }
}

impl UiApp for LibraryApp {
    fn init(&mut self, ui: &mut Ui, cx: &mut AppContext<'_>) {
        crate::fonts::install(ui);
        apply_window_icon(cx.window());
        let dark = system_dark(cx.window());
        let scale = cx.scale();
        let screen = cx.screen();
        let context = Arc::clone(cx.renderer().context());
        let waker = cx.waker();
        let wake = worker::Wake::new(move || waker.wake());
        match ArchiveApp::attached(dark, context, wake.clone(), scale, (screen.w, screen.h)) {
            Ok(app) => {
                self.app = Some(app);
                self.deadline = Some(Deadline::start(wake));
                self.last_dark = Some(dark);
            }
            Err(error) => fatal(&format!(
                "Forbidden Library could not open its library.\n\n{error}\n\nYour files were not modified."
            )),
        }
    }

    fn view(&mut self, ui: &mut Ui, cx: &mut AppContext<'_>) {
        let Some(app) = self.app.as_mut() else { return };
        let now = Instant::now();
        let dark = system_dark(cx.window());
        if self.last_dark != Some(dark) {
            self.last_dark = Some(dark);
            app.set_system_dark(dark);
        }
        let scale = cx.scale();
        let screen = cx.screen();
        let input = ui.input().clone();
        let wants_more = app.runner_frame(ui, &input, scale, (screen.w, screen.h), now);
        if wants_more {
            cx.request_frame();
        }
        if let Some(deadline) = self.deadline.as_ref() {
            deadline.set(app.next_wake(now));
        }
    }

    fn files_dropped(&mut self, paths: Vec<PathBuf>, cx: &mut AppContext<'_>) {
        if let Some(app) = self.app.as_mut() {
            app.dropped(paths);
            cx.request_frame();
        }
    }

    fn close_requested(&mut self, _cx: &mut AppContext<'_>) -> bool {
        // Unlocked collections are sealed before the window goes, so no key
        // outlives it.
        if let Some(app) = self.app.as_mut() {
            app.lock_for_exit();
        }
        true
    }
}

/// Wakes the runner when one of the app's own timers falls due — a caret
/// blink, a video bar fading out, the idle countdown. The runner knows only
/// about Dream's timers; anything the app schedules has to be posted from a
/// thread, which is exactly what [`dream::ui::app::AppWaker`] is for.
struct Deadline {
    tx: Sender<Option<Instant>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Deadline {
    fn start(wake: worker::Wake) -> Self {
        let (tx, rx) = channel::<Option<Instant>>();
        let thread = std::thread::Builder::new()
            .name("forbiddenlibrary-timer".into())
            .spawn(move || {
                let mut deadline: Option<Instant> = None;
                loop {
                    let next = match deadline {
                        Some(at) => {
                            match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                                Ok(next) => next,
                                Err(RecvTimeoutError::Timeout) => {
                                    deadline = None;
                                    wake.wake();
                                    continue;
                                }
                                // The app is gone.
                                Err(RecvTimeoutError::Disconnected) => return,
                            }
                        }
                        None => match rx.recv() {
                            Ok(next) => next,
                            Err(_) => return,
                        },
                    };
                    match next {
                        Some(at) => deadline = Some(at),
                        // The app is gone.
                        None => return,
                    }
                }
            })
            .ok();
        Self { tx, thread }
    }

    fn set(&self, at: Option<Instant>) {
        let _ = self.tx.send(at);
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        // Closing the channel ends the thread's wait.
        let _ = self.tx.send(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn system_dark(window: &Window) -> bool {
    window.theme().is_none_or(|theme| theme == Theme::Dark)
}

/// The window icon, from the PNG the installer uses. Wayland takes the icon
/// from the desktop entry instead, so this is for X11.
#[cfg(not(windows))]
fn apply_window_icon(window: &Window) {
    const PNG: &[u8] = include_bytes!("../../../assets/icon/app-icon.png");
    let Ok(image) = image::load_from_memory_with_format(PNG, image::ImageFormat::Png) else {
        return;
    };
    let (width, height) = (image.width(), image.height());
    let Ok(icon) = winit::window::Icon::from_rgba(image.into_rgba8().into_raw(), width, height)
    else {
        return;
    };
    window.set_window_icon(Some(icon));
}

/// Gives the window the icon embedded in the executable. Windows picks the
/// taskbar and Alt+Tab icon from the large one and the title bar from the
/// small one, so both are loaded at their system sizes.
#[cfg(windows)]
fn apply_window_icon(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ICON_BIG, ICON_SMALL, IMAGE_ICON, LoadImageW, SendMessageW, WM_SETICON,
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
    // The build script embeds the icon as resource 1.
    let resource = std::ptr::without_provenance::<u16>(1);
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let big = LoadImageW(module, resource, IMAGE_ICON, 256, 256, 0);
        let small = LoadImageW(module, resource, IMAGE_ICON, 16, 16, 0);
        if !big.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as isize);
        }
        if !small.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as isize);
        }
    }
}

fn fatal(message: &str) {
    eprintln!("{message}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let title: Vec<u16> = "Forbidden Library\0".encode_utf16().collect();
        let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(not(windows))]
    {
        // A window can fail before there is anything on screen to show the
        // message in; a dialog is the only place a desktop user would look.
        if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let _ = rfd::MessageDialog::new()
                .set_title("Forbidden Library")
                .set_description(message)
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
    }
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if updater::run_before_launch(&args) {
        return;
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("Forbidden Library {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|arg| arg == "--selfcheck") {
        match ArchiveApp::selfcheck() {
            Ok(summary) => println!("{summary}"),
            Err(error) => {
                eprintln!("selfcheck failed: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(index) = args.iter().position(|arg| arg == "--backup") {
        let Some(dest) = args.get(index + 1) else {
            eprintln!("--backup needs a destination folder");
            std::process::exit(2);
        };
        let settings = settings::Settings::load();
        let result = archive_core::Library::open(settings.library_root())
            .and_then(|library| library.backup(dest));
        match result {
            Ok(report) => {
                println!(
                    "Backed up {} items and {} files ({}) to {}",
                    report.items,
                    report.files,
                    archive_core::human_bytes(report.bytes),
                    report.path
                );
            }
            Err(error) => {
                eprintln!("backup failed: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Forbidden Library {}", env!("CARGO_PKG_VERSION"));
        println!("Usage: forbidden-library [--selfcheck] [--backup <folder>] [--version]");
        return;
    }

    if args.iter().any(|arg| arg == "--rendercheck") {
        std::process::exit(rendercheck(&args));
    }

    // A local photo library is not a game: prefer the integrated GPU when the
    // machine has one, and fall back to whatever GPU is there otherwise.
    let options = AppOptions {
        title: "Forbidden Library".into(),
        size: (1440.0, 900.0),
        min_size: Some((1040.0, 660.0)),
        ..Default::default()
    }
    .app_id("forbidden-library")
    .low_power_gpu(true);
    if let Err(error) = run(options, LibraryApp::new()) {
        fatal(&format!(
            "Forbidden Library stopped unexpectedly.\n\n{error}"
        ));
    }
}

/// The `--rendercheck` harness. It keeps its own tiny window loop because it
/// scripts frames: every page is drawn a set number of times and read back,
/// which the runner has no way to drive from outside.
struct Harness {
    app: Option<ArchiveApp>,
    sources: Vec<PathBuf>,
    failed: bool,
    proxy: EventLoopProxy<()>,
}

impl ApplicationHandler for Harness {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes()
            .with_title("Forbidden Library render check")
            .with_inner_size(LogicalSize::new(1440.0f64, 900.0f64))
            .with_visible(false);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("rendercheck: the window could not be created: {error}");
                self.failed = true;
                event_loop.exit();
                return;
            }
        };
        let proxy = self.proxy.clone();
        let wake = worker::Wake::new(move || {
            let _ = proxy.send_event(());
        });
        match ArchiveApp::new(window, wake) {
            Ok(mut app) => {
                if let Err(error) = run_rendercheck(&mut app, &self.sources) {
                    eprintln!("rendercheck: {error}");
                    self.failed = true;
                }
                self.app = Some(app);
            }
            Err(error) => {
                eprintln!("rendercheck: the library would not open: {error}");
                self.failed = true;
            }
        }
        event_loop.exit();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {}
}

/// Builds the sample library the check renders, runs the check, and returns
/// the process exit code.
fn rendercheck(args: &[String]) -> i32 {
    let root = std::env::temp_dir().join(format!(
        "forbiddenlibrary-rendercheck-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&root);
    if let Err(error) = std::fs::create_dir_all(root.join("media")) {
        eprintln!("rendercheck could not prepare its library: {error}");
        return 1;
    }
    // SAFETY: set before any thread reads the environment.
    unsafe {
        std::env::set_var("FORBIDDEN_LIBRARY_DATA_DIR", &root);
    }
    let palette = [
        (220u8, 226u8, 232u8),
        (35u8, 38u8, 44u8),
        (188u8, 174u8, 150u8),
        (120u8, 139u8, 156u8),
        (238u8, 205u8, 180u8),
        (74u8, 86u8, 70u8),
    ];
    let mut sources = Vec::new();
    for index in 0..8u32 {
        // Sample 0 is larger than the viewer's 2048 px preview, so the
        // largest-still check actually reaches the full-resolution decode.
        let (width, height) = if index == 0 {
            (2400, 1600)
        } else {
            (120 + (index % 3) * 40, 90 + (index % 2) * 40)
        };
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let (r, g, b) = palette[index as usize % palette.len()];
        for (position, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let x = position as u32 % width;
            let y = position as u32 / width;
            pixel[0] = (r as u32 + x / 6).min(255) as u8;
            pixel[1] = (g as u32 + y / 8).min(255) as u8;
            pixel[2] = b;
            pixel[3] = 255;
        }
        let path = root.join("media").join(format!("frame-{index}.png"));
        if std::fs::write(&path, dream::png::encode(width, height, &pixels)).is_err() {
            eprintln!("rendercheck could not write its sample images.");
            return 1;
        }
        sources.push(path);
    }
    // Extra media for the viewer checks, such as a video and an animated
    // GIF, which the generated stills do not cover.
    if let Some(extra) = std::env::var_os("FORBIDDEN_LIBRARY_RENDERCHECK_MEDIA") {
        match std::fs::read_dir(&extra) {
            Ok(entries) => {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.is_file())
                    .collect();
                files.sort();
                sources.extend(files);
            }
            Err(error) => {
                eprintln!("rendercheck could not read its media folder: {error}");
                return 1;
            }
        }
    }
    let _ = args;

    let event_loop = match EventLoop::<()>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("rendercheck could not start: {error}");
            return 1;
        }
    };
    let proxy = event_loop.create_proxy();
    let mut harness = Harness {
        app: None,
        sources,
        failed: false,
        proxy,
    };
    if let Err(error) = event_loop.run_app(&mut harness) {
        eprintln!("rendercheck stopped unexpectedly: {error}");
        let _ = std::fs::remove_dir_all(&root);
        return 1;
    }
    let failed = harness.failed;
    drop(harness);
    let _ = std::fs::remove_dir_all(root);
    i32::from(failed)
}

/// Imports the samples, draws every page and the viewer, and checks the
/// captures are not blank. Returns what went wrong, if anything.
fn run_rendercheck(app: &mut ArchiveApp, sources: &[PathBuf]) -> Result<(), String> {
    // Frames go straight to the swapchain unless capture is on, and the
    // check reads them back.
    if let Some(renderer) = app.renderer.as_mut() {
        renderer.set_frame_capture(true);
    }
    let report = archive_core::import_paths(
        &app.library,
        sources,
        &archive_core::ImportOptions::default(),
        &mut |_| {},
    );
    if report.imported == 0 && report.duplicates == 0 {
        return Err(format!("no media imported ({report:?})"));
    }
    app.reload();
    let pages = [
        app::Page::Media,
        app::Page::Albums,
        app::Page::Collections,
        app::Page::Trash,
        app::Page::Settings,
    ];
    let mut checked = Vec::new();
    for page in pages {
        app.set_page(page.clone());
        for _ in 0..42 {
            app.render(Instant::now());
            std::thread::sleep(Duration::from_millis(8));
        }
        let Some(renderer) = app.renderer.as_mut() else {
            return Err("renderer missing".into());
        };
        let (pixels, width, height) = renderer.capture_rgba8();
        if width == 0 || height == 0 || pixels.len() != (width * height * 4) as usize {
            return Err(format!("bad capture {width}x{height}"));
        }
        let stats = pixel_stats(&pixels);
        let dir = std::env::var_os("FORBIDDEN_LIBRARY_RENDERCHECK_DIR").map(PathBuf::from);
        if let Some(dir) = &dir {
            let _ = std::fs::create_dir_all(dir);
        }
        save_capture(
            dir.as_deref(),
            &format!("page-{page:?}"),
            &pixels,
            width,
            height,
        )?;
        let minimum = if page.is_media() { 0.002 } else { 0.004 };
        if stats.colors < 10 || stats.ink_fraction < minimum {
            return Err(format!("{page:?} looks blank ({stats:?})"));
        }
        checked.push((page, stats));
    }
    for (page, stats) in &checked {
        println!(
            "rendercheck {page:?}: {} colours, {:.1}% ink, background {:02x}{:02x}{:02x}",
            stats.colors,
            stats.ink_fraction * 100.0,
            stats.background.0,
            stats.background.1,
            stats.background.2
        );
    }
    app.check_import_dialogs()?;
    println!(
        "rendercheck ok · {} pages and import dialogs rendered",
        checked.len()
    );

    // The viewer: a video, an animation, a still and the largest still,
    // each opened and watched for a moment.
    check_viewers(app)
}

#[derive(Debug)]
struct PixelStats {
    colors: usize,
    ink_fraction: f32,
    background: (u8, u8, u8),
}

/// Opens the viewer on one item of each kind and checks what it draws. A
/// video or animation must advance between two captures 400 ms apart, which
/// is read from the frames the viewer has put on screen. A still must not be
/// blank, and the largest still must reach its full-resolution decode.
fn check_viewers(app: &mut ArchiveApp) -> Result<(), String> {
    app.set_page(app::Page::Media);
    for _ in 0..10 {
        app.render(Instant::now());
        std::thread::sleep(Duration::from_millis(8));
    }
    let dir = std::env::var_os("FORBIDDEN_LIBRARY_RENDERCHECK_DIR").map(PathBuf::from);
    if let Some(dir) = &dir {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let items = app.display.clone();
    let is_animated_ext =
        |ext: &str| matches!(ext.to_ascii_lowercase().as_str(), "gif" | "apng" | "webp");
    let video = items
        .iter()
        .position(|item| item.kind == archive_core::Kind::Video);
    let gif = items.iter().position(|item| {
        item.kind == archive_core::Kind::Image && item.ext.eq_ignore_ascii_case("gif")
    });
    let still = items
        .iter()
        .position(|item| item.kind == archive_core::Kind::Image && !is_animated_ext(&item.ext));
    let largest = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.kind == archive_core::Kind::Image)
        .max_by_key(|(_, item)| item.width.unwrap_or(0).max(item.height.unwrap_or(0)))
        .map(|(index, _)| index);

    let mut lines = Vec::new();
    match video {
        Some(index) => {
            lines.push(check_motion(app, index, "video", dir.as_deref())?);
            lines.push(check_video_pause(app, index)?);
        }
        None => lines.push("rendercheck viewer video: skipped (no video imported)".into()),
    }
    match gif {
        Some(index) => lines.push(check_motion(app, index, "animation", dir.as_deref())?),
        None => lines.push("rendercheck viewer animation: skipped (no GIF imported)".into()),
    }
    if let Some(index) = still {
        lines.push(check_still(app, index, "still", false, dir.as_deref())?);
    }
    // Music opens in the video engine with no picture stream, so the viewer must
    // still draw its cover, or a glyph when there is none. The track chosen has
    // a cover where one was imported, and its details panel is captured too.
    let music = items
        .iter()
        .position(|item| {
            item.kind == archive_core::Kind::Audio
                && item.thumb_state == archive_core::ThumbState::Ready
        })
        .or_else(|| {
            items
                .iter()
                .position(|item| item.kind == archive_core::Kind::Audio)
        });
    match music {
        Some(index) => {
            lines.push(check_still(app, index, "music", false, dir.as_deref())?);
            lines.push(check_details(app, index, dir.as_deref())?);
        }
        None => lines.push("rendercheck viewer music: skipped (no music imported)".into()),
    }
    if let Some(index) = largest {
        lines.push(check_still(
            app,
            index,
            "largest still",
            true,
            dir.as_deref(),
        )?);
        lines.push(check_zoom(app)?);
    }
    lines.push(check_music_advance(app)?);
    app.close_viewer();
    for line in lines {
        println!("{line}");
    }
    println!("rendercheck viewer ok");
    Ok(())
}

/// A track that ends hands over to the next track in its album, so an
/// album plays through. Runs only when the fixture album is imported, and takes
/// as long as the first track does.
fn check_music_advance(app: &mut ArchiveApp) -> Result<String, String> {
    // The album holding the first track is the list the track plays from.
    let key = app
        .library
        .music()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|(item, _)| item.original_name == "alpha-1.mp3")
        .and_then(|(_, audio)| audio.as_ref().and_then(archive_core::album_key));
    let Some(key) = key else {
        return Ok("rendercheck music advance: skipped (no alpha-1.mp3 imported)".into());
    };
    app.set_page(app::Page::Album(key));
    for _ in 0..10 {
        app.render(Instant::now());
        std::thread::sleep(Duration::from_millis(8));
    }
    let Some(first) = app
        .display
        .iter()
        .position(|item| item.original_name == "alpha-1.mp3")
    else {
        return Ok("rendercheck music advance: skipped (no alpha-1.mp3 imported)".into());
    };
    app.open_viewer(first);
    let started = Instant::now();
    loop {
        run_viewer_for(app, Duration::from_millis(100), false)?;
        match app.viewer_file_name().as_deref() {
            Some("alpha-2.mp3") => break,
            _ if started.elapsed() > Duration::from_secs(40) => {
                return Err(format!(
                    "music did not advance past alpha-1 (viewer shows {:?})",
                    app.viewer_file_name()
                ));
            }
            _ => {}
        }
    }
    Ok(format!(
        "rendercheck music advance: alpha-1 ended and alpha-2 started after {} s",
        started.elapsed().as_secs()
    ))
}

/// The play button, clicked with the pointer, must stop the open video. The
/// video is opened afresh and clicked early, because a clip that has played
/// to its end would restart on a click, which is the designed replay.
fn check_video_pause(app: &mut ArchiveApp, index: usize) -> Result<String, String> {
    app.open_viewer(index);
    run_viewer_for(app, Duration::ZERO, true)?;
    let (width, height) = app.screen;
    let area = ArchiveApp::viewer_area(width, height, 0.0);
    let layout = viewer::bar_layout(area, height, 0.0);
    let play = (
        layout.play.x + layout.play.w * 0.5,
        layout.play.y + layout.play.h * 0.5,
    );
    app.pointer_moved(play);
    // The bar fades in over 200 ms and a press counts only once it is half up.
    run_viewer_for(app, Duration::from_millis(300), false)?;
    app.pointer_button(true);
    run_viewer_for(app, Duration::from_millis(50), false)?;
    app.pointer_button(false);
    run_viewer_for(app, Duration::from_millis(100), false)?;
    let paused_at = app.viewer_frames_shown();
    run_viewer_for(app, Duration::from_millis(500), false)?;
    let later = app.viewer_frames_shown();
    if later - paused_at > 1 {
        return Err(format!(
            "video kept playing after its play button was clicked ({} frames)",
            later - paused_at
        ));
    }
    Ok(format!(
        "rendercheck viewer play button: clicking paused the video ({} frames in 500 ms after)",
        later - paused_at
    ))
}

/// A wheel turn over the still must zoom it in, and the zoom must ease there.
fn check_zoom(app: &mut ArchiveApp) -> Result<String, String> {
    let (width, height) = app.screen;
    let area = ArchiveApp::viewer_area(width, height, 0.0);
    app.pointer_moved((area.x + area.w * 0.5, area.y + area.h * 0.5));
    app.wheel(108.0, 0.0);
    run_viewer_for(app, Duration::from_millis(400), false)?;
    let (zoom, target) = app.viewer_zoom();
    if zoom < 1.2 || (zoom - target).abs() > 0.01 {
        return Err(format!(
            "wheel zoom did not settle (zoom {zoom:.2}, target {target:.2})"
        ));
    }
    Ok(format!(
        "rendercheck viewer zoom: wheel turn zoomed the still to {zoom:.2}x"
    ))
}

/// Renders for `span`, or until the viewer has shown its first frame.
fn run_viewer_for(app: &mut ArchiveApp, span: Duration, until_frame: bool) -> Result<(), String> {
    let start = Instant::now();
    loop {
        app.render(Instant::now());
        std::thread::sleep(Duration::from_millis(8));
        let elapsed = start.elapsed();
        if until_frame {
            if app.viewer_frames_shown() > 0 || elapsed > Duration::from_secs(8) {
                return Ok(());
            }
        } else if elapsed >= span {
            return Ok(());
        }
    }
}

fn capture(app: &mut ArchiveApp) -> Result<(Vec<u8>, u32, u32), String> {
    let renderer = app
        .renderer
        .as_mut()
        .ok_or_else(|| "renderer missing".to_string())?;
    let (pixels, width, height) = renderer.capture_rgba8();
    if width == 0 || height == 0 || pixels.len() != (width * height * 4) as usize {
        return Err(format!("bad capture {width}x{height}"));
    }
    Ok((pixels, width, height))
}

fn save_capture(
    dir: Option<&std::path::Path>,
    name: &str,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), String> {
    if let Some(dir) = dir {
        let path = dir.join(format!("{name}.png"));
        std::fs::write(&path, dream::png::encode(width, height, pixels))
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    }
    Ok(())
}

/// A video or animation: the viewer must show new frames between two captures.
fn check_motion(
    app: &mut ArchiveApp,
    index: usize,
    label: &str,
    dir: Option<&std::path::Path>,
) -> Result<String, String> {
    app.open_viewer(index);
    let opened = Instant::now();
    run_viewer_for(app, Duration::ZERO, true)?;
    let first_frame = opened.elapsed();
    run_viewer_for(app, Duration::from_millis(900), false)?;
    let (a, width, height) = capture(app)?;
    let shown_a = app.viewer_frames_shown();
    run_viewer_for(app, Duration::from_millis(400), false)?;
    let (b, _, _) = capture(app)?;
    let shown_b = app.viewer_frames_shown();
    run_viewer_for(app, Duration::from_millis(200), false)?;
    let stats = pixel_stats(&a);
    if stats.colors < 10 || stats.ink_fraction < 0.002 {
        return Err(format!("{label} looks blank ({stats:?})"));
    }
    if shown_b <= shown_a {
        return Err(format!(
            "{label} did not advance ({shown_a} then {shown_b} frames shown)"
        ));
    }
    if a == b {
        return Err(format!("{label} captures are identical"));
    }
    let slug = label.replace(' ', "-");
    save_capture(dir, &format!("viewer-{slug}-a"), &a, width, height)?;
    save_capture(dir, &format!("viewer-{slug}-b"), &b, width, height)?;
    Ok(format!(
        "rendercheck viewer {label}: first frame after {} ms, {} frames shown in 400 ms, captures differ, {:.1}% ink",
        first_frame.as_millis(),
        shown_b - shown_a,
        stats.ink_fraction * 100.0
    ))
}

/// The details panel of an open item, with its blocks showing what the file says.
fn check_details(
    app: &mut ArchiveApp,
    index: usize,
    dir: Option<&std::path::Path>,
) -> Result<String, String> {
    app.open_viewer(index);
    app.open_details();
    run_viewer_for(app, Duration::from_millis(900), false)?;
    let (pixels, width, height) = capture(app)?;
    let stats = pixel_stats(&pixels);
    if stats.colors < 10 || stats.ink_fraction < 0.002 {
        return Err(format!("details look blank ({stats:?})"));
    }
    save_capture(dir, "viewer-music-details", &pixels, width, height)?;
    Ok(format!(
        "rendercheck viewer music details: {} colours, {:.1}% ink",
        stats.colors,
        stats.ink_fraction * 100.0
    ))
}

/// A still must draw something. The largest still must also reach its
/// full-resolution decode, which is the only way to see the zoom source load.
fn check_still(
    app: &mut ArchiveApp,
    index: usize,
    label: &str,
    expect_full: bool,
    dir: Option<&std::path::Path>,
) -> Result<String, String> {
    app.open_viewer(index);
    let opened = Instant::now();
    loop {
        run_viewer_for(app, Duration::from_millis(50), false)?;
        let full = app.viewer_full_loaded();
        let settled = if expect_full {
            full || opened.elapsed() > Duration::from_secs(10)
        } else {
            opened.elapsed() > Duration::from_millis(600)
        };
        if settled {
            break;
        }
    }
    let full = app.viewer_full_loaded();
    if expect_full && !full {
        return Err(format!("{label}: full-resolution decode did not arrive"));
    }
    let (pixels, width, height) = capture(app)?;
    let stats = pixel_stats(&pixels);
    if stats.colors < 10 || stats.ink_fraction < 0.002 {
        return Err(format!("{label} looks blank ({stats:?})"));
    }
    let slug = label.replace(' ', "-");
    save_capture(dir, &format!("viewer-{slug}"), &pixels, width, height)?;
    Ok(format!(
        "rendercheck viewer {label}: {} colours, {:.1}% ink, full-resolution {}",
        stats.colors,
        stats.ink_fraction * 100.0,
        if full { "loaded" } else { "not loaded" }
    ))
}

fn pixel_stats(pixels: &[u8]) -> PixelStats {
    use std::collections::HashMap;
    let mut counts: HashMap<(u8, u8, u8), u32> = HashMap::new();
    let mut quantized = std::collections::HashSet::new();
    for pixel in pixels.chunks_exact(4) {
        let key = (pixel[0], pixel[1], pixel[2]);
        *counts.entry(key).or_insert(0) += 1;
        quantized.insert((pixel[0] >> 3, pixel[1] >> 3, pixel[2] >> 3));
    }
    let total = (pixels.len() / 4).max(1);
    let (background, count) = counts
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(color, count)| (*color, *count))
        .unwrap_or(((255, 255, 255), 0));
    PixelStats {
        colors: quantized.len(),
        ink_fraction: 1.0 - count as f32 / total as f32,
        background,
    }
}
