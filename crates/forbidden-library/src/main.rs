//! Forbidden Library — the app.
//!
//! The window hosts the immediate-mode UI in [`app`] on Dream's Vulkan
//! renderer. All storage work happens on background threads; the event loop
//! only renders when a frame can have changed, and workers wake it through
//! the event-loop proxy.
//!
//! ```text
//! forbidden-library                 # the app
//! forbidden-library --selfcheck     # headless UI + engine check, prints a summary
//! forbidden-library --rendercheck   # headless render of every page and the viewer
//! forbidden-library --backup <dir>  # consistent copy of the whole library
//! ```

#![cfg_attr(
    all(target_os = "windows", not(feature = "console")),
    windows_subsystem = "windows"
)]

mod app;
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
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::window::{CursorIcon, Theme, Window, WindowId};

struct Host {
    app: Option<ArchiveApp>,
    window: Option<Arc<Window>>,
    proxy: EventLoopProxy<()>,
    dropped: VecDeque<PathBuf>,
    last_drop: Option<Instant>,
    cursor: Option<CursorIcon>,
    rendercheck: bool,
    /// Set when a rendercheck step fails, so the process exits non-zero.
    rendercheck_failed: bool,
    sources: Vec<PathBuf>,
    /// The window is fully covered by other windows (platforms that report it).
    occluded: bool,
    /// The hidden state last acted on, so a hide or show is handled once.
    hidden_applied: bool,
    /// When the last frame was drawn, for pacing the next one.
    last_render: Instant,
    /// The monitor's refresh period and when it was read.
    refresh: Option<(Instant, Duration)>,
    /// When to return the working set to the OS: after warm-up and quiet spells.
    memory: dream::memory::IdleTrim,
}

/// The fastest the UI animates. A high-refresh monitor would otherwise ask for
/// several times the frames of a 60 Hz one for the same motion; 120 Hz is
/// already past what UI motion needs.
const MOTION_FPS: f32 = 120.0;

impl Host {
    fn new(proxy: EventLoopProxy<()>, rendercheck: bool, sources: Vec<PathBuf>) -> Self {
        Self {
            app: None,
            window: None,
            proxy,
            dropped: VecDeque::new(),
            last_drop: None,
            cursor: None,
            rendercheck,
            rendercheck_failed: false,
            sources,
            occluded: false,
            hidden_applied: false,
            last_render: Instant::now(),
            refresh: None,
            memory: dream::memory::IdleTrim::new(),
        }
    }

    /// Minimised or fully covered: nothing drawn could be seen.
    fn hidden(&self) -> bool {
        self.occluded
            || self
                .window
                .as_ref()
                .and_then(|window| window.is_minimized())
                .unwrap_or(false)
    }

    /// Act on a hide or show once. A hidden window draws nothing, so the
    /// renderer's idle buffers and the process's cold pages go back now.
    fn apply_hidden(&mut self, hidden: bool) {
        if self.hidden_applied == hidden {
            return;
        }
        self.hidden_applied = hidden;
        if hidden {
            if let Some(renderer) = self.app.as_mut().and_then(|app| app.renderer.as_mut()) {
                renderer.release_scene_resources();
                renderer.trim_memory();
            }
            dream::memory::trim_working_set();
            self.memory = dream::memory::IdleTrim::new();
        } else {
            self.request_redraw();
        }
    }

    /// Time between frames while something moves: one per refresh of the
    /// window's monitor, capped at [`MOTION_FPS`]. Pacing in software, rather
    /// than letting the swapchain block, keeps some drivers from busy-waiting a
    /// whole core inside the present wait.
    fn frame_period(&mut self, now: Instant) -> Duration {
        if let Some((read, period)) = self.refresh
            && now.duration_since(read) < Duration::from_millis(500)
        {
            return period;
        }
        let hz = self
            .window
            .as_ref()
            .and_then(|window| window.current_monitor())
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .map(|millihertz| millihertz as f32 / 1000.0)
            .filter(|hz| hz.is_finite() && *hz > 1.0)
            .unwrap_or(60.0)
            .min(MOTION_FPS);
        let period = Duration::from_secs_f32(1.0 / hz.clamp(10.0, 1000.0));
        self.refresh = Some((now, period));
        period
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn flush_drops(&mut self) {
        if self.dropped.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = self.dropped.drain(..).collect();
        if let Some(app) = self.app.as_mut() {
            app.dropped(paths);
        }
        self.last_drop = None;
        self.request_redraw();
    }

    fn run_rendercheck(&mut self, event_loop: &ActiveEventLoop) {
        let Some(app) = self.app.as_mut() else { return };
        // Frames go straight to the swapchain unless capture is on, and the
        // check reads them back.
        if let Some(renderer) = app.renderer.as_mut() {
            renderer.set_frame_capture(true);
        }
        let report = archive_core::import_paths(
            &app.library,
            &self.sources,
            &archive_core::ImportOptions::default(),
            &mut |_| {},
        );
        if report.imported == 0 && report.duplicates == 0 {
            eprintln!("rendercheck: no media imported ({report:?})");
            self.rendercheck_failed = true;
            event_loop.exit();
            return;
        }
        app.reload();
        let pages = [
            app::Page::Media,
            app::Page::Albums,
            app::Page::Collections,
            app::Page::Trash,
        ];
        let mut checked = Vec::new();
        for page in pages {
            app.set_page(page.clone());
            for _ in 0..42 {
                app.render(Instant::now());
                std::thread::sleep(Duration::from_millis(8));
            }
            let Some(renderer) = app.renderer.as_mut() else {
                eprintln!("rendercheck: renderer missing");
                self.rendercheck_failed = true;
                event_loop.exit();
                return;
            };
            let (pixels, width, height) = renderer.capture_rgba8();
            if width == 0 || height == 0 || pixels.len() != (width * height * 4) as usize {
                eprintln!("rendercheck: bad capture {width}x{height}");
                self.rendercheck_failed = true;
                event_loop.exit();
                return;
            }
            let stats = pixel_stats(&pixels);
            let dir = std::env::var_os("FORBIDDEN_LIBRARY_RENDERCHECK_DIR").map(PathBuf::from);
            if let Some(dir) = &dir {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(error) = save_capture(
                dir.as_deref(),
                &format!("page-{page:?}"),
                &pixels,
                width,
                height,
            ) {
                eprintln!("rendercheck: {error}");
                self.rendercheck_failed = true;
                event_loop.exit();
                return;
            }
            let minimum = if page.is_media() { 0.002 } else { 0.004 };
            if stats.colors < 10 || stats.ink_fraction < minimum {
                eprintln!("rendercheck: {page:?} looks blank ({:?})", stats);
                self.rendercheck_failed = true;
                event_loop.exit();
                return;
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
        if let Err(error) = app.check_import_dialogs() {
            eprintln!("rendercheck import dialogs: {error}");
            self.rendercheck_failed = true;
            event_loop.exit();
            return;
        }
        println!(
            "rendercheck ok · {} pages and import dialogs rendered",
            checked.len()
        );

        // The viewer: a video, an animation, a still and the largest still,
        // each opened and watched for a moment.
        if let Err(error) = check_viewers(app) {
            eprintln!("rendercheck viewer: {error}");
            self.rendercheck_failed = true;
            event_loop.exit();
            return;
        }
        event_loop.exit();
    }

    fn update_cursor(&mut self) {
        let Some(app) = self.app.as_ref() else { return };
        let wanted = if app.cursor_hand {
            CursorIcon::Pointer
        } else {
            CursorIcon::Default
        };
        if self.cursor != Some(wanted) {
            self.cursor = Some(wanted);
            if let Some(window) = &self.window {
                window.set_cursor(wanted);
            }
        }
    }
}

/// Gives the window the icon embedded in the executable. Windows picks the
/// taskbar and Alt+Tab icon from the large one and the title bar from the
/// small one, so both are loaded at their system sizes.
#[cfg(windows)]
fn apply_exe_icon(window: &Window) {
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

#[cfg(not(windows))]
fn apply_exe_icon(_window: &Window) {}

impl ApplicationHandler for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes()
            .with_title("Forbidden Library")
            .with_inner_size(LogicalSize::new(1440.0f64, 900.0f64))
            .with_min_inner_size(LogicalSize::new(1040.0f64, 660.0f64))
            .with_visible(!self.rendercheck);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                fatal(&format!("The window could not be created.\n\n{error}"));
                return;
            }
        };
        window.set_ime_allowed(true);
        apply_exe_icon(&window);
        self.window = Some(Arc::clone(&window));
        match ArchiveApp::new(Arc::clone(&window), self.proxy.clone()) {
            Ok(app) => {
                self.app = Some(app);
                if self.rendercheck {
                    self.run_rendercheck(event_loop);
                } else {
                    window.request_redraw();
                }
            }
            Err(error) => {
                fatal(&format!(
                    "Forbidden Library could not open its library.\n\n{error}\n\nYour files were not modified."
                ));
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Read before `app` borrows `self` for the rest of the handler.
        let hidden = self.hidden();
        let Some(app) = self.app.as_mut() else {
            return;
        };
        // Pointer, wheel, key and drop activity inside the window keeps unlocked
        // collections open. Other events, such as the pointer leaving, do not.
        if matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::KeyboardInput { .. }
                | WindowEvent::DroppedFile(_)
        ) {
            app.note_activity();
        }
        match event {
            WindowEvent::CloseRequested => {
                // Unlocked collections are sealed before the window goes, so no
                // key outlives it.
                app.lock_for_exit();
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                if hidden {
                    // Nothing can be seen: keep the timers honest, draw nothing.
                    app.idle_tick(now);
                    return;
                }
                app.render(now);
                self.last_render = now;
                self.memory.activity(now);
                // The next frame, if one is wanted, is scheduled in
                // `about_to_wait`, paced to the display.
                self.update_cursor();
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
            }
            WindowEvent::Resized(size) => {
                // Minimised (or otherwise zero-sized) windows have nothing to
                // draw into: keep the old swapchain until a real size arrives.
                if size.width == 0 || size.height == 0 {
                    return;
                }
                if let Some(renderer) = app.renderer.as_mut() {
                    renderer.resize(size);
                }
                app.resize((size.width, size.height));
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                app.set_scale(scale_factor as f32);
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    if size.width > 0 && size.height > 0 {
                        if let Some(renderer) = app.renderer.as_mut() {
                            renderer.resize(size);
                        }
                        app.resize((size.width, size.height));
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::ThemeChanged(theme) => {
                app.set_system_dark(theme == Theme::Dark);
                self.request_redraw();
            }
            WindowEvent::Focused(true) => {
                app.observe_theme();
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = app.scale.max(0.5);
                app.pointer_moved((position.x as f32 / scale, position.y as f32 / scale));
                self.request_redraw();
            }
            WindowEvent::CursorLeft { .. } => {
                app.pointer_moved((-10_000.0, -10_000.0));
                self.request_redraw();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                app.pointer_button(state == ElementState::Pressed);
                self.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                app.pointer_secondary();
                self.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                match delta {
                    MouseScrollDelta::LineDelta(_, y) => app.wheel(y * 54.0, 0.0),
                    MouseScrollDelta::PixelDelta(position) => app.wheel(0.0, position.y as f32),
                }
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                app.modifiers(state.control_key(), state.shift_key());
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(key) = map_key(&event) {
                    app.key(key, event.state == ElementState::Pressed);
                }
                if event.state == ElementState::Pressed
                    && !app.input.ctrl
                    && let Some(text) = event.text.as_deref()
                    && !text.chars().any(|c| c.is_control())
                {
                    app.text_input(text);
                }
                self.request_redraw();
            }
            WindowEvent::DroppedFile(path) => {
                self.dropped.push_back(path);
                self.last_drop = Some(Instant::now());
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(150),
                ));
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        self.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let hidden = self.hidden();
        self.apply_hidden(hidden);
        if hidden {
            // Only the unlocked-collection timer matters while hidden; video
            // and animation wait for the window to show again.
            if let Some(app) = self.app.as_mut() {
                if app.hidden_wake().is_some_and(|at| at <= now) {
                    app.idle_tick(now);
                }
                match app.hidden_wake() {
                    Some(at) if at > now => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
                    _ => event_loop.set_control_flow(ControlFlow::Wait),
                }
            }
            return;
        }
        if self.memory.trim_if_due(now) {
            dream::memory::trim_working_set();
        }
        let mut drop_due = None;
        if let Some(last) = self.last_drop {
            let due = last + Duration::from_millis(150);
            if now >= due {
                self.flush_drops();
            } else {
                drop_due = Some(due);
            }
        }
        let (wants, wake) = match self.app.as_ref() {
            Some(app) => (app.wants_frame(), app.next_wake(now)),
            None => (false, None),
        };
        if let Some(due) = drop_due {
            // While a drop is being debounced, only a wake-up that is due may
            // redraw. Frames that merely want to run wait for the drop to settle.
            if wake.is_some_and(|at| at <= now) {
                self.request_redraw();
            }
            let wait = match wake {
                Some(at) if at > now => at.min(due),
                _ => due,
            };
            event_loop.set_control_flow(ControlFlow::WaitUntil(wait));
            return;
        }
        if wants {
            let due = self.last_render + self.frame_period(now);
            if now >= due {
                self.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(due));
            }
            return;
        }
        if wake.is_some_and(|at| at <= now) {
            self.request_redraw();
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        match wake.into_iter().chain(self.memory.next_deadline()).min() {
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

fn map_key(event: &winit::event::KeyEvent) -> Option<dream::ui::widgets::Key> {
    use dream::ui::widgets::Key;
    match &event.logical_key {
        WinitKey::Named(NamedKey::Escape) => Some(Key::Escape),
        WinitKey::Named(NamedKey::Enter) => Some(Key::Enter),
        WinitKey::Named(NamedKey::Tab) => Some(Key::Tab),
        WinitKey::Named(NamedKey::Backspace) => Some(Key::Backspace),
        WinitKey::Named(NamedKey::Delete) => Some(Key::Delete),
        WinitKey::Named(NamedKey::ArrowLeft) => Some(Key::Left),
        WinitKey::Named(NamedKey::ArrowRight) => Some(Key::Right),
        WinitKey::Named(NamedKey::ArrowUp) => Some(Key::Up),
        WinitKey::Named(NamedKey::ArrowDown) => Some(Key::Down),
        WinitKey::Named(NamedKey::Home) => Some(Key::Home),
        WinitKey::Named(NamedKey::End) => Some(Key::End),
        WinitKey::Named(NamedKey::PageUp) => Some(Key::PageUp),
        WinitKey::Named(NamedKey::PageDown) => Some(Key::PageDown),
        WinitKey::Named(NamedKey::Space) => Some(Key::Char(' ')),
        WinitKey::Character(text) => text.chars().next().map(Key::Char),
        _ => None,
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

    let rendercheck = args.iter().any(|arg| arg == "--rendercheck");
    let mut check_root = None;
    let mut sources = Vec::new();
    if rendercheck {
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
            fatal(&format!(
                "rendercheck could not prepare its library: {error}"
            ));
            return;
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
                fatal("rendercheck could not write its sample images.");
                return;
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
                    fatal(&format!(
                        "rendercheck could not read its media folder: {error}"
                    ));
                    return;
                }
            }
        }
        check_root = Some(root);
    }

    let event_loop = match EventLoop::<()>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            fatal(&format!("The app could not start.\n\n{error}"));
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut host = Host::new(proxy, rendercheck, sources);
    if let Err(error) = event_loop.run_app(&mut host) {
        fatal(&format!(
            "Forbidden Library stopped unexpectedly.\n\n{error}"
        ));
    }
    let rendercheck_failed = host.rendercheck_failed;
    drop(host);
    if let Some(root) = check_root {
        let _ = std::fs::remove_dir_all(root);
    }
    if rendercheck_failed {
        std::process::exit(1);
    }
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
