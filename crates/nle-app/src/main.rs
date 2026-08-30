//! Native Maelstrom shell. Release Windows builds are a GUI subsystem process so
//! launching the exe does not open a console in front of the splash.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod model_preload;
mod phase1_ui;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use nle_project_io::{ProjectDocument, ProjectSettings};
use nle_render::{
    HubRenderer, MAX_COLOR_CORRECTIONS_PER_LAYER, RectInstance, SplashRenderer, SplashRgba,
    TextureInstance, TexturedRect, TimelineRectCallbackHandle, TimelineTextureCallbackHandle,
    ViewerColorCorrection, ViewerColorCurve, ViewerCompositorCallbackHandle, ViewerFrame,
    ViewerLayerPrimitive, ViewerRgbCurves,
};
use nle_ui_core::{
    ActivePreviewDecoderBackend, ActivePreviewDiagnostic, ActivePreviewFallbackReason,
    ActivePreviewSourceKind, EditorAction, EditorProjectSnapshot, EditorState, HubAction,
    HubBackdrops, Language, LivePipelineTiming, LivePipelineTimingRepresentative,
    LivePipelineTimingSample, LivePipelineTimingStage, MediaKind, MonitorFrame, PreviewQuality,
    ProjectFrameRate, ProjectHubState, ProxyMediaStatus, RuntimeDiagnostics, TimelineCanvas,
    ViewerCanvas, classify_path, configure_fonts, show_editor_with_canvases, show_with_backdrops,
};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition},
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{CursorIcon, Icon, Window, WindowAttributes, WindowId, WindowLevel},
};

mod hardware;
use hardware::{HardwareProfile, MachineProfile};

const ENGLISH_SPLASH: &[u8] = include_bytes!("../../../assets/splash/english.png");
const JAPANESE_SPLASH: &[u8] = include_bytes!("../../../assets/splash/japanese.png");
const APP_ICON: &[u8] = include_bytes!("../../../assets/branding/maelstrom-window-icon.png");
const MIN_SPLASH_VISIBLE: Duration = Duration::from_millis(2400);
const MIN_MONITOR_CACHE_MB: usize = 512;
const MAX_MONITOR_CACHE_MB: usize = 2 * 1024;
const MONITOR_LAYER_COUNT: usize = nle_ui_core::PREVIEW_VIDEO_LAYER_COUNT;
/// One reserved sequential decode lane per visible video layer plus one bounded speculative
/// prewarm/reverse-scrub budget across all layers.
const MONITOR_FOREGROUND_SESSION_CAP: usize = MONITOR_LAYER_COUNT;
const MONITOR_BACKGROUND_SESSION_CAP: usize = MONITOR_LAYER_COUNT;
/// Bounded audio metadata captured alongside the video request. This is scheduling state only:
/// native audio transport still receives every audible target.
const MAX_PREVIEW_AUDIO_SOURCES: usize = 64;
/// A bounded source overview used for timeline thumbnails, project artwork, and instant scrub
/// proxies. Exact monitor decoding still replaces a proxy as soon as it is ready.
const SCRUB_PREVIEW_TARGET_FPS: f64 = 30.0;
const SCRUB_PREVIEW_MAX_FRAMES: usize = 1024;
const SCRUB_PREVIEW_MIN_FRAMES: usize = 12;
// Four dense 16:9 atlases remain within the same approximate 256 MiB CPU budget as four old
// 180px/256-frame atlases (each is about 56.25 MiB at the cap).
const SCRUB_PREVIEW_FRAME_HEIGHT: u32 = 90;
const MAX_RUNTIME_VIDEO_STRIPS: usize = 4;
const MAX_RUNTIME_VIDEO_STRIP_BYTES: usize = 256 * 1024 * 1024;

/// Disposable user proxies live outside project truth. The bounded proxy crate owns pruning.
fn proxy_cache_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Maelstrom")
        .join("Proxy Media")
}
/// The finite Phase 0 pressure checkpoint uses five 70 MiB strips. Four candidates would occupy
/// 280 MiB before the byte cap evicts the oldest retained strip.
#[cfg(test)]
const PHASE0_VIDEO_STRIP_BYTES: usize = 70 * 1024 * 1024;
#[cfg(test)]
const PHASE0_VIDEO_STRIP_WIDTH: u32 = 7_168;
#[cfg(test)]
const PHASE0_VIDEO_STRIP_HEIGHT: u32 = 2_560;
const STILL_PREVIEW_MAX_WIDTH: u32 = 320;
const STILL_IMAGE_MAX_PIXELS: u64 = 100_000_000;
const PROJECT_CATALOG_VERSION: u32 = 1;
/// Coalesce a continuous trim/scrub/layout drag into one project serialization.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(250);
const AUTO_PREVIEW_SLOW_SAMPLES: u8 = 4;
const AUTO_PREVIEW_FAST_SAMPLES: u16 = 90;
const DEFAULT_PLAYBACK_SOAK_SECONDS: u64 = 600;
const MAX_PLAYBACK_SOAK_SECONDS: u64 = 3_600;
#[cfg(test)]
const DEFAULT_PHASE1_SUSTAINED_SOAK_SECONDS: u64 = 600;
#[cfg(test)]
const MIN_PHASE1_SUSTAINED_SOAK_SECONDS: u64 = 15;
#[cfg(test)]
const MAX_PHASE1_SUSTAINED_SOAK_SECONDS: u64 = 3_600;
#[cfg(test)]
const DEFAULT_PHASE1_LIVE_AUDIO_SECONDS: u64 = 5;
#[cfg(test)]
const MIN_PHASE1_LIVE_AUDIO_SECONDS: u64 = 2;
#[cfg(test)]
const MAX_PHASE1_LIVE_AUDIO_SECONDS: u64 = 30;
#[cfg(test)]
const PHASE1_LIVE_AUDIO_WARMUP_RESERVE_SECONDS: u64 = 10;

fn monitor_cache_bytes_from_args(args: impl IntoIterator<Item = String>) -> usize {
    let mut args = args.into_iter();
    let mut requested_mb = None;
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--cache-mb=") {
            requested_mb = value.parse::<usize>().ok();
        } else if arg == "--cache-mb" {
            requested_mb = args.next().and_then(|value| value.parse::<usize>().ok());
        }
    }
    requested_mb
        .unwrap_or(nle_decode::DEFAULT_FRAME_CACHE_BYTES / (1024 * 1024))
        .clamp(MIN_MONITOR_CACHE_MB, MAX_MONITOR_CACHE_MB)
        .saturating_mul(1024 * 1024)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase0SurfaceAdapterClass {
    IntegratedGpu,
    DiscreteGpu,
}

impl Phase0SurfaceAdapterClass {
    fn device_type(self) -> wgpu::DeviceType {
        match self {
            Self::IntegratedGpu => wgpu::DeviceType::IntegratedGpu,
            Self::DiscreteGpu => wgpu::DeviceType::DiscreteGpu,
        }
    }
}

fn parse_phase0_surface_adapter_class(value: &str) -> Result<Phase0SurfaceAdapterClass, String> {
    match value {
        "IntegratedGpu" => Ok(Phase0SurfaceAdapterClass::IntegratedGpu),
        "DiscreteGpu" => Ok(Phase0SurfaceAdapterClass::DiscreteGpu),
        _ => Err(format!(
            "MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS must be IntegratedGpu or DiscreteGpu, got {value:?}"
        )),
    }
}

fn phase0_surface_adapter_class_from_environment()
-> Result<Option<Phase0SurfaceAdapterClass>, String> {
    match std::env::var("MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS") {
        Ok(value) => parse_phase0_surface_adapter_class(&value).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS must be valid Unicode".to_owned())
        }
    }
}

fn select_phase0_surface_adapter(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'_>,
    class: Phase0SurfaceAdapterClass,
) -> Result<wgpu::Adapter, String> {
    let required_type = class.device_type();
    let power_preference = match class {
        Phase0SurfaceAdapterClass::IntegratedGpu => wgpu::PowerPreference::LowPower,
        Phase0SurfaceAdapterClass::DiscreteGpu => wgpu::PowerPreference::HighPerformance,
    };
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference,
        compatible_surface: Some(surface),
        force_fallback_adapter: false,
    }))
    .map_err(|error| {
        format!(
            "MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS={class:?} could not request a surface-compatible DX12 adapter: {error}"
        )
    })?;
    let info = adapter.get_info();
    if info.device_type != required_type {
        return Err(format!(
            "MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS={class:?} requires DX12 {required_type:?}, but wgpu selected {} (vendor=0x{:04X}, device=0x{:04X}, type={:?}, driver={}); refusing fallback",
            info.name, info.vendor, info.device, info.device_type, info.driver
        ));
    }
    Ok(adapter)
}

fn logical_cursor_position(
    screen_x: i32,
    screen_y: i32,
    client_origin: PhysicalPosition<i32>,
    scale_factor: f64,
) -> egui::Pos2 {
    let scale = scale_factor.max(f64::EPSILON) as f32;
    egui::Pos2::new(
        (screen_x - client_origin.x) as f32 / scale,
        (screen_y - client_origin.y) as f32 / scale,
    )
}

/// Converts winit logical window coordinates into egui points.
fn egui_point_from_winit_logical(
    point: egui::Pos2,
    window_scale_factor: f64,
    pixels_per_point: f32,
) -> egui::Pos2 {
    point * (window_scale_factor as f32 / pixels_per_point.max(f32::EPSILON))
}

fn current_cursor_in_egui_points(window: &Window, context: &egui::Context) -> Option<egui::Pos2> {
    current_cursor_in_window(window).map(|point| {
        egui_point_from_winit_logical(point, window.scale_factor(), context.pixels_per_point())
    })
}

fn clear_text_focus_for_screen_change(context: &egui::Context) {
    // A Project Hub search/name field can otherwise retain keyboard ownership after the editor
    // replaces that screen. Timeline shortcuts must work immediately after opening a project.
    context.memory_mut(|memory| memory.stop_text_input());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeEditorShortcut {
    Undo,
    Redo,
    Razor,
    Delete,
    CommandPalette,
}

fn native_editor_shortcut(key: &Key, modifiers: ModifiersState) -> Option<NativeEditorShortcut> {
    let primary = modifiers.control_key() || modifiers.super_key();
    let shift = modifiers.shift_key();
    if matches!(key, Key::Named(NamedKey::Delete)) && !primary {
        return Some(NativeEditorShortcut::Delete);
    }
    if !primary {
        return None;
    }
    let Key::Character(character) = key else {
        return None;
    };
    if character.eq_ignore_ascii_case("z") {
        return Some(if shift {
            NativeEditorShortcut::Redo
        } else {
            NativeEditorShortcut::Undo
        });
    }
    if character.eq_ignore_ascii_case("y") {
        return Some(NativeEditorShortcut::Redo);
    }
    if character.eq_ignore_ascii_case("b") {
        return Some(NativeEditorShortcut::Razor);
    }
    if character.eq_ignore_ascii_case("p") {
        return Some(NativeEditorShortcut::CommandPalette);
    }
    None
}

#[derive(Default)]
struct MediaDragPointer {
    primary_button_held: bool,
    cursor: Option<egui::Pos2>,
    press_origin: Option<egui::Pos2>,
    media_drag_owned: bool,
}

impl MediaDragPointer {
    fn cursor_moved(&mut self, point: egui::Pos2) -> Option<egui::Pos2> {
        self.cursor = Some(point);
        (self.primary_button_held && !self.media_drag_owned)
            .then(|| self.press_origin.unwrap_or(point))
    }

    fn primary_pressed(&mut self, point: Option<egui::Pos2>) -> Option<egui::Pos2> {
        self.primary_button_held = true;
        self.media_drag_owned = false;
        // GetCursorPos is sampled synchronously for the button event and is therefore the
        // authoritative press location. A retained CursorMoved position can lag behind when
        // Windows coalesces pointer motion, which previously made a visible Media Pool press
        // miss the source card and left the timeline drop inert.
        self.press_origin = point.or(self.cursor);
        self.cursor = point.or(self.cursor);
        self.press_origin
    }

    fn media_drag_claimed(&mut self) {
        self.media_drag_owned = true;
    }

    fn primary_released(&mut self) -> Option<egui::Pos2> {
        self.primary_button_held = false;
        self.press_origin = None;
        self.media_drag_owned = false;
        self.cursor
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn format_file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn kraken_ffmpeg() -> PathBuf {
    bundled_media_tool("ffmpeg")
}

fn bundled_media_tool(name: &str) -> PathBuf {
    let executable_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(&executable_name)))
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(executable_name))
}

fn preferred_h264_encoders(profile: Option<&HardwareProfile>) -> Vec<nle_export::H264Encoder> {
    let mut encoders = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let _ = profile;
        encoders.push(nle_export::H264Encoder::VideoToolbox);
    }
    #[cfg(not(target_os = "macos"))]
    {
        if profile.is_some_and(|profile| profile.intel_quick_sync_candidate) {
            encoders.push(nle_export::H264Encoder::IntelQuickSync);
        }
        if let Some(profile) = profile {
            if profile
                .adapters
                .iter()
                .any(|adapter| adapter.vendor == 0x10de)
            {
                encoders.push(nle_export::H264Encoder::Nvidia);
            }
            if profile
                .adapters
                .iter()
                .any(|adapter| adapter.vendor == 0x1002)
            {
                encoders.push(nle_export::H264Encoder::Amd);
            }
        }
        encoders.push(nle_export::H264Encoder::MediaFoundation);
        encoders.push(nle_export::H264Encoder::OpenH264);
    }
    encoders
}

fn observe_encoder_backend(observed: &mut Option<String>, encoder: nle_export::H264Encoder) {
    *observed = Some(encoder.ffmpeg_name().to_owned());
}

#[cfg(windows)]
fn current_cursor_in_window(window: &Window) -> Option<egui::Pos2> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let mut cursor = POINT { x: 0, y: 0 };
    // SAFETY: `GetCursorPos` only writes to the valid stack-allocated POINT.
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return None;
    }
    let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    let mut client_origin = POINT { x: 0, y: 0 };
    // SAFETY: the HWND belongs to `window`; ClientToScreen only updates the valid POINT.
    if unsafe {
        windows_sys::Win32::Graphics::Gdi::ClientToScreen(
            handle.hwnd.get() as _,
            &mut client_origin,
        )
    } == 0
    {
        return None;
    }
    Some(logical_cursor_position(
        cursor.x,
        cursor.y,
        winit::dpi::PhysicalPosition::new(client_origin.x, client_origin.y),
        window.scale_factor(),
    ))
}

#[cfg(not(windows))]
fn current_cursor_in_window(_window: &Window) -> Option<egui::Pos2> {
    None
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProjectCatalog {
    version: u32,
    projects: Vec<CatalogProject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CatalogProject {
    id: u32,
    name: String,
    recent: String,
    size: String,
    #[serde(default)]
    path: Option<PathBuf>,
}

impl From<CatalogProject> for nle_ui_core::Project {
    fn from(project: CatalogProject) -> Self {
        Self {
            id: project.id,
            name: project.name,
            recent: project.recent,
            size: project.size,
            thumbnail: None,
        }
    }
}

#[derive(Clone, Debug)]
struct ThumbnailRgba {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
struct SaveRequest {
    project_path: PathBuf,
    document: ProjectDocument,
    thumbnail: Option<(PathBuf, ThumbnailRgba)>,
}

#[derive(Clone, Debug)]
struct CatalogSaveRequest {
    path: PathBuf,
    catalog: ProjectCatalog,
}

#[derive(Default)]
struct AutosaveSchedule {
    pending_generation: Option<u64>,
    deadline: Option<Instant>,
}

impl AutosaveSchedule {
    /// Returns true when a snapshot should be made now. Initial, forced, and thumbnail saves
    /// bypass the quiet period; a new generation restarts it.
    fn ready(
        &mut self,
        last_enqueued_generation: Option<u64>,
        generation: u64,
        has_thumbnail: bool,
        force: bool,
        now: Instant,
    ) -> bool {
        if force || has_thumbnail || last_enqueued_generation.is_none() {
            self.clear();
            return true;
        }
        if last_enqueued_generation == Some(generation) {
            self.clear();
            return false;
        }
        if self.pending_generation != Some(generation) {
            self.pending_generation = Some(generation);
            self.deadline = Some(now + AUTOSAVE_DEBOUNCE);
            return false;
        }
        self.deadline.is_none_or(|deadline| now >= deadline)
    }

    fn clear(&mut self) {
        self.pending_generation = None;
        self.deadline = None;
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

struct ProjectWriter {
    pending: Arc<Mutex<HashMap<PathBuf, SaveRequest>>>,
    wake_tx: mpsc::SyncSender<WriterCommand>,
    success_rx: mpsc::Receiver<WriterSuccess>,
    error_rx: mpsc::Receiver<WriterFailure>,
    join: Option<thread::JoinHandle<()>>,
}

struct WriterSuccess {
    project_path: PathBuf,
    file_size: u64,
}

struct WriterFailure {
    message: String,
    request: SaveRequest,
}

enum WriterCommand {
    Wake,
    Flush(mpsc::Sender<()>),
    Shutdown(mpsc::Sender<()>),
}

impl ProjectWriter {
    #[cfg(test)]
    fn new() -> Self {
        Self::new_with_notifier(|| {})
    }

    fn new_with_notifier(notify: impl Fn() + Send + Sync + 'static) -> Self {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (success_tx, success_rx) = mpsc::channel();
        let (error_tx, error_rx) = mpsc::channel();
        let worker_pending = Arc::clone(&pending);
        let join = thread::Builder::new()
            .name("maelstrom-project-writer".into())
            .spawn(move || {
                while let Ok(command) = wake_rx.recv() {
                    let save = || {
                        let requests = {
                            let mut pending = worker_pending.lock().expect("project writer lock");
                            std::mem::take(&mut *pending)
                        };
                        for (_, request) in requests {
                            match persist_project_document(&request) {
                                Ok(()) => {
                                    if let Ok(metadata) = fs::metadata(&request.project_path) {
                                        let _ = success_tx.send(WriterSuccess {
                                            project_path: request.project_path,
                                            file_size: metadata.len(),
                                        });
                                        notify();
                                    }
                                }
                                Err(error) => {
                                    let failure = request.clone();
                                    retain_failed_save(
                                        &mut worker_pending.lock().expect("project writer lock"),
                                        request,
                                    );
                                    let _ = error_tx.send(WriterFailure {
                                        message: error.to_string(),
                                        request: failure,
                                    });
                                    notify();
                                }
                            }
                        }
                    };
                    match command {
                        WriterCommand::Wake => save(),
                        WriterCommand::Flush(done) => {
                            save();
                            let _ = done.send(());
                        }
                        WriterCommand::Shutdown(done) => {
                            save();
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            })
            .expect("start project writer");
        Self {
            pending,
            wake_tx,
            success_rx,
            error_rx,
            join: Some(join),
        }
    }

    fn save_latest(&self, request: SaveRequest) {
        let mut pending = self.pending.lock().expect("project writer lock");
        coalesce_save_request(&mut pending, request);
        drop(pending);
        let _ = self.wake_tx.try_send(WriterCommand::Wake);
    }

    fn flush(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        if self.wake_tx.send(WriterCommand::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    fn flush_and_shutdown(&mut self) {
        if self.join.is_none() {
            return;
        }
        let (done_tx, done_rx) = mpsc::channel();
        let _ = self.wake_tx.send(WriterCommand::Shutdown(done_tx));
        let _ = done_rx.recv();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct CatalogWriter {
    pending: Arc<Mutex<Option<CatalogSaveRequest>>>,
    wake_tx: mpsc::SyncSender<WriterCommand>,
    error_rx: mpsc::Receiver<CatalogWriterFailure>,
    join: Option<thread::JoinHandle<()>>,
}

struct CatalogWriterFailure {
    message: String,
    request: CatalogSaveRequest,
}

impl CatalogWriter {
    #[cfg(test)]
    fn new() -> Self {
        Self::new_with_notifier(|| {})
    }

    fn new_with_notifier(notify: impl Fn() + Send + Sync + 'static) -> Self {
        let pending = Arc::new(Mutex::new(None));
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (error_tx, error_rx) = mpsc::channel();
        let worker_pending = Arc::clone(&pending);
        let join = thread::Builder::new()
            .name("maelstrom-catalog-writer".into())
            .spawn(move || {
                while let Ok(command) = wake_rx.recv() {
                    let save = || {
                        let request = worker_pending.lock().expect("catalog writer lock").take();
                        if let Some(request) = request
                            && let Err(error) = persist_catalog_request(&request)
                        {
                            let _ = error_tx.send(CatalogWriterFailure {
                                message: error.to_string(),
                                request,
                            });
                            notify();
                        }
                    };
                    match command {
                        WriterCommand::Wake => save(),
                        WriterCommand::Flush(done) => {
                            save();
                            let _ = done.send(());
                        }
                        WriterCommand::Shutdown(done) => {
                            save();
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            })
            .expect("start catalog writer");
        Self {
            pending,
            wake_tx,
            error_rx,
            join: Some(join),
        }
    }

    fn save_latest(&self, request: CatalogSaveRequest) {
        *self.pending.lock().expect("catalog writer lock") = Some(request);
        let _ = self.wake_tx.try_send(WriterCommand::Wake);
    }

    #[cfg(test)]
    fn flush(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        if self.wake_tx.send(WriterCommand::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    fn flush_and_shutdown(&mut self) {
        if self.join.is_none() {
            return;
        }
        let (done_tx, done_rx) = mpsc::channel();
        let _ = self.wake_tx.send(WriterCommand::Shutdown(done_tx));
        let _ = done_rx.recv();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn coalesce_save_request(pending: &mut HashMap<PathBuf, SaveRequest>, mut request: SaveRequest) {
    if request.thumbnail.is_none()
        && let Some(previous) = pending.get_mut(&request.project_path)
    {
        request.thumbnail = previous.thumbnail.take();
    }
    pending.insert(request.project_path.clone(), request);
}

fn retain_failed_save(pending: &mut HashMap<PathBuf, SaveRequest>, mut failed: SaveRequest) {
    match pending.entry(failed.project_path.clone()) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            // A save queued while the failed request was being written is newer. Never replace
            // its document, but retain a thumbnail that has not reached disk yet.
            if entry.get().thumbnail.is_none() {
                entry.get_mut().thumbnail = failed.thumbnail.take();
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(failed);
        }
    }
}

fn project_root(catalog_path: &std::path::Path) -> PathBuf {
    catalog_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf()
        .join("projects")
}

fn project_document_path(catalog_path: &std::path::Path, project_id: u32) -> PathBuf {
    project_root(catalog_path)
        .join(project_id.to_string())
        .join("project.nleproj")
}

fn legacy_project_document_path(catalog_path: &std::path::Path, project_id: u32) -> PathBuf {
    project_root(catalog_path)
        .join(project_id.to_string())
        .join("project.json")
}

fn project_thumbnail_path(catalog_path: &std::path::Path, project_id: u32) -> PathBuf {
    project_root(catalog_path)
        .join(project_id.to_string())
        .join("thumbnail.png")
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    nle_project_io::atomic_write(path, bytes)
}

fn load_project_document(path: &std::path::Path) -> Result<Option<ProjectDocument>, String> {
    nle_project_io::read_document(path).map_err(|error| error.to_string())
}

fn scrub_preview_frame_count(duration_seconds: f64) -> usize {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return SCRUB_PREVIEW_MIN_FRAMES;
    }
    (duration_seconds * SCRUB_PREVIEW_TARGET_FPS).ceil().clamp(
        SCRUB_PREVIEW_MIN_FRAMES as f64,
        SCRUB_PREVIEW_MAX_FRAMES as f64,
    ) as usize
}

fn timeline_texture_id(project_epoch: u64, media_id: u32) -> u64 {
    project_epoch.wrapping_shl(32) | u64::from(media_id)
}

fn crop_video_strip_frame(strip: &nle_waveform::VideoStrip, frame: usize) -> Option<ThumbnailRgba> {
    if strip.frame_count == 0
        || frame >= strip.frame_count
        || strip.columns == 0
        || strip.rows == 0
        || strip.frame_width == 0
        || strip.frame_height == 0
    {
        return None;
    }
    let columns = u32::try_from(strip.columns).ok()?;
    let rows = u32::try_from(strip.rows).ok()?;
    if strip.frame_count > strip.columns.checked_mul(strip.rows)? {
        return None;
    }
    let width = strip.frame_width.checked_mul(columns)?;
    let height = strip.frame_height.checked_mul(rows)?;
    let atlas_bytes = usize::try_from(width)
        .ok()?
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if strip.width != width || strip.height != height || strip.rgba.len() != atlas_bytes {
        return None;
    }
    let column = frame % strip.columns;
    let row = frame / strip.columns;
    if row >= strip.rows {
        return None;
    }
    let frame_bytes = usize::try_from(strip.frame_width)
        .ok()?
        .checked_mul(strip.frame_height as usize)?
        .checked_mul(4)?;
    let row_bytes = usize::try_from(strip.frame_width).ok()?.checked_mul(4)?;
    let mut rgba = vec![0; frame_bytes];
    for y in 0..strip.frame_height as usize {
        let source = ((row * strip.frame_height as usize + y) * strip.width as usize
            + column * strip.frame_width as usize)
            * 4;
        let target = y * row_bytes;
        rgba[target..target + row_bytes].copy_from_slice(&strip.rgba[source..source + row_bytes]);
    }
    Some(ThumbnailRgba {
        width: strip.frame_width,
        height: strip.frame_height,
        rgba,
    })
}

fn crop_representative_frame(strip: &nle_waveform::VideoStrip) -> Option<ThumbnailRgba> {
    crop_video_strip_frame(strip, strip.frame_count / 2)
}

fn video_strip_sample_tick(strip: &nle_waveform::VideoStrip, frame: usize) -> Option<i64> {
    if frame >= strip.frame_count
        || !strip.duration_seconds.is_finite()
        || strip.duration_seconds <= 0.0
    {
        return None;
    }
    let duration_ticks = (strip.duration_seconds * 1_000_000.0).round();
    if !(1.0..=i64::MAX as f64).contains(&duration_ticks) {
        return None;
    }
    let tick = (duration_ticks as u128)
        .checked_mul(frame as u128)?
        .checked_div(strip.frame_count as u128)?;
    i64::try_from(tick).ok()
}

fn nearest_video_strip_frame_index(
    strip: &nle_waveform::VideoStrip,
    source_tick: i64,
) -> Option<usize> {
    if strip.frame_count == 0
        || !strip.duration_seconds.is_finite()
        || strip.duration_seconds <= 0.0
    {
        return None;
    }
    let duration_ticks = (strip.duration_seconds * 1_000_000.0).round();
    if !(1.0..=i64::MAX as f64).contains(&duration_ticks) {
        return None;
    }
    let scaled = (source_tick.max(0) as u128).checked_mul(strip.frame_count as u128)?;
    let nearest = scaled
        .checked_add(duration_ticks as u128 / 2)?
        .checked_div(duration_ticks as u128)?;
    usize::try_from(nearest)
        .ok()
        .map(|index| index.min(strip.frame_count.saturating_sub(1)))
}

type ScrubProxyKey = (u32, usize);

fn should_present_scrub_proxy(last: Option<ScrubProxyKey>, next: ScrubProxyKey) -> bool {
    last != Some(next)
}

fn should_retain_close_full_monitor_frame(
    current: Option<(u32, i64, u32, u32)>,
    media_id: u32,
    target_source_tick: i64,
    full_size: (u32, u32),
    source_frame_tolerance: Option<i64>,
) -> bool {
    current.is_some_and(|(current_media_id, current_tick, width, height)| {
        let source_frame_tolerance = source_frame_tolerance.unwrap_or_default();
        current_media_id == media_id
            && (width, height) == full_size
            && current_tick.abs_diff(target_source_tick) <= source_frame_tolerance.max(0) as u64
    })
}

fn scrub_proxy_allows_monitor_frame(
    proxy_active: bool,
    latest_request_id: u64,
    candidate_request_id: u64,
) -> bool {
    !proxy_active || candidate_request_id == latest_request_id
}

fn persist_project_document(request: &SaveRequest) -> io::Result<()> {
    nle_project_io::write_document(&request.project_path, &request.document)
        .map_err(io::Error::other)?;
    if let Some((path, image)) = &request.thumbnail {
        let mut png = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut png),
            &image.rgba,
            image.width,
            image.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(io::Error::other)?;
        atomic_write(path, &png)?;
    }
    Ok(())
}

impl From<&nle_ui_core::Project> for CatalogProject {
    fn from(project: &nle_ui_core::Project) -> Self {
        Self {
            id: project.id,
            name: project.name.clone(),
            recent: project.recent.clone(),
            size: project.size.clone(),
            path: None,
        }
    }
}

fn project_catalog_path() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local").join("share"))
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join("Maelstrom").join("projects.json")
}

fn catalog_backup_path(path: &std::path::Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    PathBuf::from(backup)
}

#[cfg(test)]
fn load_catalog(path: &std::path::Path) -> Vec<nle_ui_core::Project> {
    load_catalog_with_paths(path).0
}

fn load_catalog_with_paths(
    path: &std::path::Path,
) -> (Vec<nle_ui_core::Project>, HashMap<u32, PathBuf>) {
    for candidate in [path.to_path_buf(), catalog_backup_path(path)] {
        let Ok(bytes) = fs::read(candidate) else {
            continue;
        };
        let Ok(catalog) = serde_json::from_slice::<ProjectCatalog>(&bytes) else {
            continue;
        };
        if catalog.version == PROJECT_CATALOG_VERSION {
            let mut paths = HashMap::new();
            let projects = catalog
                .projects
                .into_iter()
                .map(|project| {
                    if let Some(path) = &project.path {
                        paths.insert(project.id, path.clone());
                    }
                    project.into()
                })
                .collect();
            return (projects, paths);
        }
    }
    (Vec::new(), HashMap::new())
}

#[cfg(test)]
fn persist_catalog(path: &std::path::Path, projects: &[nle_ui_core::Project]) -> io::Result<()> {
    persist_catalog_with_paths(path, projects, &HashMap::new())
}

#[cfg(test)]
fn persist_catalog_with_paths(
    path: &std::path::Path,
    projects: &[nle_ui_core::Project],
    project_paths: &HashMap<u32, PathBuf>,
) -> io::Result<()> {
    persist_catalog_request(&CatalogSaveRequest {
        path: path.to_path_buf(),
        catalog: project_catalog_snapshot(projects, project_paths),
    })
}

fn project_catalog_snapshot(
    projects: &[nle_ui_core::Project],
    project_paths: &HashMap<u32, PathBuf>,
) -> ProjectCatalog {
    ProjectCatalog {
        version: PROJECT_CATALOG_VERSION,
        projects: projects
            .iter()
            .map(|project| {
                let mut entry = CatalogProject::from(project);
                entry.path = project_paths.get(&project.id).cloned();
                entry
            })
            .collect(),
    }
}

fn persist_catalog_request(request: &CatalogSaveRequest) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&request.catalog)
        .map_err(|error| io::Error::other(format!("serialize project catalog: {error}")))?;
    nle_project_io::atomic_write(&request.path, &bytes)
}

struct MediaAnalysisResult {
    project_epoch: u64,
    media_id: u32,
    is_still: bool,
    metadata: Result<nle_waveform::MediaMetadata, String>,
    frame_timing: Result<nle_waveform::FrameTiming, String>,
    waveform: Result<nle_waveform::Waveform, String>,
    video_strip: Result<nle_waveform::VideoStrip, String>,
}

struct StillImageAnalysis {
    strip: nle_waveform::VideoStrip,
    source_width: u32,
    source_height: u32,
}

fn analyze_still_image(
    path: &Path,
    cancellation: &AtomicBool,
) -> Result<StillImageAnalysis, String> {
    if cancellation.load(Ordering::Acquire) {
        return Err("still-image analysis cancelled".to_owned());
    }
    let dimensions_reader = image::ImageReader::open(path)
        .map_err(|error| format!("could not open still image: {error}"))?
        .with_guessed_format()
        .map_err(|error| format!("could not identify still image: {error}"))?;
    let (source_width, source_height) = dimensions_reader
        .into_dimensions()
        .map_err(|error| format!("could not read still-image dimensions: {error}"))?;
    let pixels = u64::from(source_width).saturating_mul(u64::from(source_height));
    if source_width == 0 || source_height == 0 || pixels > STILL_IMAGE_MAX_PIXELS {
        return Err(format!(
            "still image dimensions {source_width}x{source_height} exceed the analysis limit"
        ));
    }
    let image = image::ImageReader::open(path)
        .map_err(|error| format!("could not open still image: {error}"))?
        .with_guessed_format()
        .map_err(|error| format!("could not identify still image: {error}"))?
        .decode()
        .map_err(|error| format!("could not decode still image: {error}"))?;
    if cancellation.load(Ordering::Acquire) {
        return Err("still-image analysis cancelled".to_owned());
    }
    let thumbnail = image.thumbnail(STILL_PREVIEW_MAX_WIDTH, SCRUB_PREVIEW_FRAME_HEIGHT);
    let rgba = thumbnail.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(StillImageAnalysis {
        strip: nle_waveform::VideoStrip {
            width,
            height,
            rgba: rgba.into_raw(),
            duration_seconds: nle_ui_core::DEFAULT_STILL_IMAGE_DURATION.0 as f64 / 1_000_000.0,
            frame_count: 1,
            frame_width: width,
            frame_height: height,
            columns: 1,
            rows: 1,
        },
        source_width,
        source_height,
    })
}

enum ProjectDialogResult {
    Opened {
        known_id: Option<u32>,
        path: PathBuf,
        language: Language,
        file_size: Option<u64>,
        document: Result<Option<Box<ProjectDocument>>, String>,
    },
    Exported(Result<PathBuf, String>),
    VideoExportDestination(Option<PathBuf>),
    KrakenUpscaleDestination(Option<PathBuf>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MonitorRequestKey {
    project_epoch: u64,
    media_id: u32,
    source_tick: i64,
    width: u32,
    height: u32,
    is_scrubbing: bool,
    prewarm_scrub_workers: bool,
    high_quality_scaling: bool,
    selected_quality: PreviewQuality,
    resolved_quality: PreviewQuality,
    source_frame_rate: Option<nle_ui_core::SourceFrameRate>,
    source_frame_duration_tick: Option<i64>,
}

const fn active_preview_decoder_backend(
    backend: nle_decode::DecodeBackend,
) -> ActivePreviewDecoderBackend {
    match backend {
        nle_decode::DecodeBackend::Software => ActivePreviewDecoderBackend::Software,
        nle_decode::DecodeBackend::IntelQuickSync => ActivePreviewDecoderBackend::IntelQuickSync,
        nle_decode::DecodeBackend::Nvidia => ActivePreviewDecoderBackend::NvidiaCuvid,
        nle_decode::DecodeBackend::VideoToolbox => ActivePreviewDecoderBackend::AppleVideoToolbox,
        nle_decode::DecodeBackend::D3D11VA => ActivePreviewDecoderBackend::WindowsD3d11va,
        nle_decode::DecodeBackend::DXVA2 => ActivePreviewDecoderBackend::WindowsDxva2,
    }
}

const fn active_preview_fallback_reason(
    reason: nle_decode::DecodeFallbackReason,
) -> ActivePreviewFallbackReason {
    match reason {
        nle_decode::DecodeFallbackReason::ForcedSoftware => {
            ActivePreviewFallbackReason::ForcedSoftware
        }
        nle_decode::DecodeFallbackReason::HardwareUnavailable => {
            ActivePreviewFallbackReason::HardwareUnavailable
        }
        nle_decode::DecodeFallbackReason::HardwareDecodeFailed => {
            ActivePreviewFallbackReason::HardwareDecodeFailed
        }
    }
}

/// Decoder ownership is keyed separately from output policy.  Changing dimensions, scrub
/// quality, or scaling must keep the same source actor warm; changing media, path, or decode
/// acceleration must hand the endpoint to the correct bounded actor.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorSourceIdentity {
    media_id: u32,
    path: PathBuf,
    acceleration: nle_decode::AccelerationPreference,
}

fn monitor_source_identity_changed(
    previous: Option<&MonitorSourceIdentity>,
    media_id: u32,
    path: &Path,
    acceleration: nle_decode::AccelerationPreference,
) -> bool {
    previous.is_some_and(|previous| {
        previous.media_id != media_id
            || previous.path.as_path() != path
            || previous.acceleration != acceleration
    })
}

/// Immutable, allocation-free description of one viewer update. Decoder workers still receive
/// owned paths only when a source key actually changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewSourceRequest {
    layer: usize,
    priority: u8,
    clip_id: nle_timeline::ClipId,
    media_id: u32,
    source_tick: i64,
    source_frame_rate: Option<nle_ui_core::SourceFrameRate>,
    /// Exact local VFR frame span. CFR sources derive this from `source_frame_rate`.
    source_frame_duration_tick: Option<i64>,
}

/// Lightweight audio scheduling metadata. Paths, effect stacks, and gains remain owned by the
/// audio transport; the preview scheduler only needs source identity and ordered timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewAudioSourceRequest {
    priority: u8,
    track_id: nle_timeline::TrackId,
    clip_id: nle_timeline::ClipId,
    media_id: u32,
    source_tick: i64,
    clip_tick: i64,
    transition_role: Option<nle_ui_core::AudioPlaybackTransitionRole>,
}

#[derive(Debug, PartialEq, Eq)]
struct PreviewRequest {
    sequence_generation: u64,
    playhead_tick: i64,
    is_scrubbing: bool,
    output_size: [u32; 2],
    selected_quality: PreviewQuality,
    resolved_quality: PreviewQuality,
    sources: [Option<PreviewSourceRequest>; MONITOR_LAYER_COUNT],
    audio_sources: [Option<PreviewAudioSourceRequest>; MAX_PREVIEW_AUDIO_SOURCES],
    audio_source_count: usize,
}

impl PreviewRequest {
    fn audio_sources_truncated(&self) -> bool {
        self.audio_source_count > MAX_PREVIEW_AUDIO_SOURCES
    }
}

/// Returns contributing video layers in decode-admission order: highest source priority first,
/// with the visually topmost layer winning ties. The fixed four-slot result avoids allocation on
/// the monitor submission hot path.
fn contributing_video_layers_by_priority(
    sources: &[Option<PreviewSourceRequest>; MONITOR_LAYER_COUNT],
) -> ([usize; MONITOR_LAYER_COUNT], usize) {
    let mut layers = [0; MONITOR_LAYER_COUNT];
    let mut count = 0;
    for (layer, source) in sources.iter().enumerate() {
        let Some(source) = source else {
            continue;
        };
        let mut insert_at = count;
        while insert_at > 0 {
            let preceding_layer = layers[insert_at - 1];
            let preceding = sources[preceding_layer]
                .expect("admission order contains only contributing layers");
            if preceding.priority > source.priority
                || (preceding.priority == source.priority && preceding_layer > layer)
            {
                break;
            }
            layers[insert_at] = preceding_layer;
            insert_at -= 1;
        }
        layers[insert_at] = layer;
        count += 1;
    }
    (layers, count)
}

/// Selects one complete lower-priority physical source group for eviction. Multiple logical
/// layers may share one coordinator actor, so returning only one layer would not release the
/// group's hard permit. A group needed by any equal/higher-priority contributor is protected.
fn lower_priority_monitor_eviction_group(
    sources: &[Option<PreviewSourceRequest>; MONITOR_LAYER_COUNT],
    identities: &[Option<MonitorSourceIdentity>; MONITOR_LAYER_COUNT],
    deferred: &[bool; MONITOR_LAYER_COUNT],
    latest_request_ids: &[u64; MONITOR_LAYER_COUNT],
    requester_layer: usize,
    requester_identity: &MonitorSourceIdentity,
) -> [bool; MONITOR_LAYER_COUNT] {
    let Some(requester) = sources[requester_layer] else {
        return [false; MONITOR_LAYER_COUNT];
    };
    let mut selected_layer = None;
    let mut selected_rank = (u8::MAX, u64::MAX, usize::MAX);

    for candidate_layer in 0..MONITOR_LAYER_COUNT {
        let Some(candidate_identity) = identities[candidate_layer].as_ref() else {
            continue;
        };
        if candidate_identity == requester_identity {
            continue;
        }
        if identities[..candidate_layer]
            .iter()
            .any(|identity| identity.as_ref() == Some(candidate_identity))
        {
            continue;
        }

        let mut has_live_member = false;
        let mut group_priority = 0;
        let mut group_recency = 0;
        let mut group_top_layer = 0;
        for layer in 0..MONITOR_LAYER_COUNT {
            if identities[layer].as_ref() != Some(candidate_identity) {
                continue;
            }
            has_live_member |= !deferred[layer];
            group_recency = group_recency.max(latest_request_ids[layer]);
            if let Some(source) = sources[layer] {
                group_priority = group_priority.max(source.priority);
                group_top_layer = group_top_layer.max(layer);
            }
        }
        if !has_live_member || group_priority >= requester.priority {
            continue;
        }
        let rank = (group_priority, group_recency, group_top_layer);
        if selected_layer.is_none() || rank < selected_rank {
            selected_layer = Some(candidate_layer);
            selected_rank = rank;
        }
    }

    let mut selected = [false; MONITOR_LAYER_COUNT];
    let Some(selected_identity) = selected_layer.and_then(|layer| identities[layer].as_ref())
    else {
        return selected;
    };
    for layer in 0..MONITOR_LAYER_COUNT {
        selected[layer] = identities[layer].as_ref() == Some(selected_identity);
    }
    selected
}

/// Orders only selected monitor slots for deferred retry. Higher visual priority wins; visually
/// topmost layers win ties, matching first admission and preventing an evicted lower layer from
/// stealing a newly freed source permit back on the next pump.
fn selected_monitor_layers_by_priority(
    priorities: &[u8; MONITOR_LAYER_COUNT],
    selected: &[bool; MONITOR_LAYER_COUNT],
) -> ([usize; MONITOR_LAYER_COUNT], usize) {
    let mut layers = [0; MONITOR_LAYER_COUNT];
    let mut count = 0;
    for layer in 0..MONITOR_LAYER_COUNT {
        if !selected[layer] {
            continue;
        }
        let mut insert_at = count;
        while insert_at > 0 {
            let preceding = layers[insert_at - 1];
            if priorities[preceding] > priorities[layer]
                || (priorities[preceding] == priorities[layer] && preceding > layer)
            {
                break;
            }
            layers[insert_at] = preceding;
            insert_at -= 1;
        }
        layers[insert_at] = layer;
        count += 1;
    }
    (layers, count)
}

fn preview_decode_size(editor: &EditorState, scrubbing: bool) -> (u32, u32) {
    if editor.playing || scrubbing {
        editor.monitor_playback_decode_size_hint()
    } else {
        editor.monitor_paused_decode_size_hint()
    }
}

fn progressive_scrub_frames(preview: &PreviewRequest) -> bool {
    preview.is_scrubbing
}

fn preview_request(editor: &EditorState) -> PreviewRequest {
    // During a ruler drag prioritize source-time accuracy and request turnaround. The retained
    // frame is refined at the user's selected quality as soon as the gesture ends.
    let is_scrubbing = editor.is_scrubbing();
    let (width, height) = preview_decode_size(editor, is_scrubbing);
    let mut sources = [None; MONITOR_LAYER_COUNT];
    for (layer, target) in editor.playback_targets().enumerate() {
        sources[layer] = Some(PreviewSourceRequest {
            layer,
            // Sources are ordered bottom-to-top; a larger value therefore means more visible.
            priority: layer.saturating_add(1).min(u8::MAX as usize) as u8,
            clip_id: target.clip_id,
            media_id: target.media_id,
            source_tick: target.decode_tick.0,
            source_frame_rate: target.source_frame_rate,
            source_frame_duration_tick: target.source_frame_duration_tick.map(|tick| tick.0),
        });
    }
    let mut audio_sources = [None; MAX_PREVIEW_AUDIO_SOURCES];
    let mut audio_source_count = 0_usize;
    editor.visit_audio_playback_sources(|target| {
        let priority = audio_source_count.saturating_add(1).min(u8::MAX as usize) as u8;
        if let Some(slot) = audio_sources.get_mut(audio_source_count) {
            *slot = Some(PreviewAudioSourceRequest {
                priority,
                track_id: target.track_id,
                clip_id: target.clip_id,
                media_id: target.media_id,
                source_tick: target.source_tick.0,
                clip_tick: target.clip_tick.0,
                transition_role: target.transition.map(|transition| transition.role),
            });
        }
        audio_source_count = audio_source_count.saturating_add(1);
    });
    PreviewRequest {
        sequence_generation: editor.timeline.generation(),
        playhead_tick: editor.playhead.0,
        is_scrubbing,
        output_size: [width, height],
        selected_quality: editor.preview_quality(),
        resolved_quality: editor.resolved_preview_quality(),
        sources,
        audio_sources,
        audio_source_count,
    }
}

/// Monitor seeks address source frames rather than arbitrary microseconds. This preserves 60/120
/// fps media inside a 30 fps project, prevents redundant micro-seeks, and keeps the exact same
/// frame grid when a scrub gesture is released.
fn monitor_source_tick_for_preview(
    source_tick: i64,
    source_frame_rate: Option<nle_ui_core::SourceFrameRate>,
) -> i64 {
    let source_tick = source_tick.max(0);
    let Some(source_rate) = source_frame_rate else {
        return source_tick;
    };
    let source_tick = source_tick as u128;
    let numerator = u128::from(source_rate.numerator());
    let frame_ticks = 1_000_000_u128 * u128::from(source_rate.denominator());
    let frame = source_tick.saturating_mul(numerator) / frame_ticks;
    frame
        .saturating_mul(frame_ticks)
        .div_ceil(numerator)
        .min(i64::MAX as u128) as i64
}

fn monitor_source_frame_duration_tick(
    source_frame_rate: Option<nle_ui_core::SourceFrameRate>,
    indexed_duration_tick: Option<i64>,
) -> Option<i64> {
    if let Some(duration) = indexed_duration_tick.filter(|duration| *duration > 0) {
        return Some(duration);
    }
    let source_rate = source_frame_rate?;
    let frame_ticks = 1_000_000_u128 * u128::from(source_rate.denominator());
    Some(
        frame_ticks
            .div_ceil(u128::from(source_rate.numerator()))
            .min(i64::MAX as u128) as i64,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdaptivePreviewController {
    resolved: PreviewQuality,
    source_ids: [Option<(nle_timeline::ClipId, u32)>; MONITOR_LAYER_COUNT],
    slow_samples: [u8; MONITOR_LAYER_COUNT],
    fast_samples: [u16; MONITOR_LAYER_COUNT],
    observed: [bool; MONITOR_LAYER_COUNT],
    unavailable: [bool; MONITOR_LAYER_COUNT],
}

impl Default for AdaptivePreviewController {
    fn default() -> Self {
        Self {
            resolved: PreviewQuality::Full,
            source_ids: [None; MONITOR_LAYER_COUNT],
            slow_samples: [0; MONITOR_LAYER_COUNT],
            fast_samples: [0; MONITOR_LAYER_COUNT],
            observed: [false; MONITOR_LAYER_COUNT],
            unavailable: [false; MONITOR_LAYER_COUNT],
        }
    }
}

impl AdaptivePreviewController {
    /// Resets evidence only for source slots whose identity changed. A new source cannot inherit a
    /// nearly-complete slow streak from the media that previously occupied its layer.
    fn sync_sources(
        &mut self,
        sources: [Option<PreviewSourceRequest>; MONITOR_LAYER_COUNT],
    ) -> Option<PreviewQuality> {
        for (layer, source) in sources.into_iter().enumerate() {
            let source_id = source.map(|source| (source.clip_id, source.media_id));
            if self.source_ids[layer] != source_id {
                self.source_ids[layer] = source_id;
                self.reset_layer_samples(layer);
                self.unavailable[layer] = false;
            }
        }
        if self.source_ids.iter().all(Option::is_none) && self.resolved != PreviewQuality::Full {
            self.resolved = PreviewQuality::Full;
            self.reset_all_samples();
            return Some(self.resolved);
        }
        None
    }

    /// Observe end-to-end request turnaround against the current project frame budget.
    /// Downshifts require repeated pressure; recovery deliberately takes much longer so Auto
    /// cannot visibly oscillate around a threshold or react to one cold seek.
    fn observe(
        &mut self,
        layer: usize,
        turnaround: Duration,
        frame_budget_ms: f32,
    ) -> Option<PreviewQuality> {
        if layer >= MONITOR_LAYER_COUNT || self.source_ids[layer].is_none() {
            return None;
        }
        self.observed[layer] = true;
        self.unavailable[layer] = false;
        let elapsed_ms = turnaround.as_secs_f32() * 1_000.0;
        let frame_budget_ms = frame_budget_ms.clamp(8.0, 26.0);
        if elapsed_ms > frame_budget_ms {
            self.fast_samples[layer] = 0;
            self.slow_samples[layer] = self.slow_samples[layer].saturating_add(1);
            if self.slow_samples[layer] >= AUTO_PREVIEW_SLOW_SAMPLES {
                if let Some(next) = lower_preview_quality(self.resolved) {
                    self.resolved = next;
                    self.reset_all_samples();
                    return Some(next);
                }
                self.slow_samples[layer] = 0;
            }
            return None;
        }
        if elapsed_ms <= frame_budget_ms * 0.45 {
            self.slow_samples[layer] = 0;
            self.fast_samples[layer] = self.fast_samples[layer].saturating_add(1);
            let mut eligible_sources = 0;
            let mut all_observed_sources_are_fast = true;
            for layer in 0..MONITOR_LAYER_COUNT {
                if self.source_ids[layer].is_some() && !self.unavailable[layer] {
                    eligible_sources += 1;
                    all_observed_sources_are_fast &= self.observed[layer]
                        && self.fast_samples[layer] >= AUTO_PREVIEW_FAST_SAMPLES;
                }
            }
            all_observed_sources_are_fast &= eligible_sources > 0;
            if all_observed_sources_are_fast
                && let Some(next) = higher_preview_quality(self.resolved)
            {
                self.resolved = next;
                self.reset_all_samples();
                return Some(next);
            }
            return None;
        }
        self.slow_samples[layer] = 0;
        self.fast_samples[layer] = 0;
        None
    }

    fn reset_layer_samples(&mut self, layer: usize) {
        self.slow_samples[layer] = 0;
        self.fast_samples[layer] = 0;
        self.observed[layer] = false;
    }

    fn reset_all_samples(&mut self) {
        self.slow_samples = [0; MONITOR_LAYER_COUNT];
        self.fast_samples = [0; MONITOR_LAYER_COUNT];
        self.observed = [false; MONITOR_LAYER_COUNT];
    }

    fn mark_layer_unavailable(&mut self, layer: usize) {
        self.reset_layer_samples(layer);
        self.unavailable[layer] = true;
    }
}

fn lower_preview_quality(quality: PreviewQuality) -> Option<PreviewQuality> {
    match quality {
        PreviewQuality::Auto | PreviewQuality::Full => Some(PreviewQuality::Half),
        PreviewQuality::Half => Some(PreviewQuality::Quarter),
        PreviewQuality::Quarter => Some(PreviewQuality::Eighth),
        PreviewQuality::Eighth => None,
    }
}

fn higher_preview_quality(quality: PreviewQuality) -> Option<PreviewQuality> {
    match quality {
        PreviewQuality::Eighth => Some(PreviewQuality::Quarter),
        PreviewQuality::Quarter => Some(PreviewQuality::Half),
        PreviewQuality::Half => Some(PreviewQuality::Full),
        PreviewQuality::Auto | PreviewQuality::Full => None,
    }
}

fn preview_frame_budget_ms(editor: &EditorState) -> f32 {
    // Leave headroom for upload/composite/presentation rather than allowing decode to consume the
    // entire project frame. The clamp prevents extreme rates from destabilizing Auto.
    (editor.frame_duration_tick().0.max(1) as f32 / 1_000.0 * 0.8).clamp(8.0, 26.0)
}

fn adaptive_preview_can_observe(selected: PreviewQuality, scrubbing: bool) -> bool {
    selected == PreviewQuality::Auto && !scrubbing
}

#[derive(Clone, Debug, PartialEq)]
struct AudioClipKey {
    track_id: nle_timeline::TrackId,
    clip_id: nle_timeline::ClipId,
    path: PathBuf,
    gain_db: f32,
    gain_left_db: f32,
    gain_right_db: f32,
    pan: f32,
    effects: Vec<nle_audio::AudioProcessorSpec>,
    fade_in_ticks: i64,
    fade_in_curve: f32,
    fade_out_ticks: i64,
    fade_out_curve: f32,
    clip_duration_ticks: i64,
    transition: Option<(nle_ui_core::AudioPlaybackTransitionRole, i64, i64)>,
}

fn native_audio_effects(
    effects: &[nle_timeline::AudioEffect],
) -> Vec<nle_audio::AudioProcessorSpec> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            nle_timeline::AudioEffect::Bypassed(_) => None,
            nle_timeline::AudioEffect::HighPass { hz } => {
                Some(nle_audio::AudioProcessorSpec::HighPass {
                    hz: nle_timeline::AudioEffect::effective_filter_hz(*hz),
                })
            }
            nle_timeline::AudioEffect::LowPass { hz } => {
                Some(nle_audio::AudioProcessorSpec::LowPass {
                    hz: nle_timeline::AudioEffect::effective_filter_hz(*hz),
                })
            }
            nle_timeline::AudioEffect::Eq { hz, db } => Some(nle_audio::AudioProcessorSpec::Eq {
                hz: nle_timeline::AudioEffect::effective_filter_hz(*hz),
                db: *db,
            }),
            nle_timeline::AudioEffect::StereoWidth { width } => {
                Some(nle_audio::AudioProcessorSpec::StereoWidth { width: *width })
            }
            _ => None,
        })
        .collect()
}

fn same_audio_lane_identity(left: &[AudioClipKey], right: &[AudioClipKey]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.track_id == right.track_id
                && left.clip_id == right.clip_id
                && left.path == right.path
        })
}

fn retained_audio_lanes_are_continuous(
    current: &AudioTransportState,
    next_keys: &[AudioClipKey],
    next_source_ticks: &[i64],
    elapsed_ticks: i64,
    tolerance_ticks: i64,
) -> bool {
    let mut retained = 0;
    for (next_key, next_source_tick) in next_keys.iter().zip(next_source_ticks) {
        let Some(index) = current.keys.iter().position(|key| {
            key.track_id == next_key.track_id
                && key.clip_id == next_key.clip_id
                && key.path == next_key.path
        }) else {
            continue;
        };
        let Some(previous_source_tick) = current.source_ticks.get(index) else {
            return false;
        };
        retained += 1;
        let expected = previous_source_tick.saturating_add(elapsed_ticks);
        if next_source_tick.saturating_sub(expected).abs() > tolerance_ticks.max(1) {
            return false;
        }
    }
    retained > 0
}

struct AudioTransportState {
    keys: Vec<AudioClipKey>,
    source_ticks: Vec<i64>,
    source_tick: i64,
    timeline_tick: i64,
    started_at: Instant,
}

fn audio_master_timeline_tick(
    timeline_tick_at_seek: i64,
    source_tick_at_seek: i64,
    consumed_source_tick: i64,
) -> i64 {
    timeline_tick_at_seek.saturating_add(consumed_source_tick.saturating_sub(source_tick_at_seek))
}

fn window_icon() -> Icon {
    let image = image::load_from_memory(APP_ICON)
        .expect("decode embedded application icon")
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("create native application icon")
}

fn decode_embedded_rgba(png: &[u8]) -> ThumbnailRgba {
    let image = image::load_from_memory(png)
        .expect("decode embedded splash art")
        .into_rgba8();
    ThumbnailRgba {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

fn load_hub_backdrop(
    ctx: &egui::Context,
    name: &'static str,
    image: &ThumbnailRgba,
) -> egui::TextureHandle {
    ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied(
            [image.width as usize, image.height as usize],
            &image.rgba,
        ),
        egui::TextureOptions::LINEAR,
    )
}

/// Bridges GPU-neutral editor geometry to the retained native rectangle callback.
/// Assembly uses an application-owned Vec and submits through one lock after egui finishes.
struct NativeTimelineCanvas<'a> {
    rect_callback: TimelineRectCallbackHandle,
    texture_callback: TimelineTextureCallbackHandle,
    rect_scratch: &'a mut Vec<RectInstance>,
    texture_scratch: &'a mut Vec<TexturedRect>,
}

impl NativeTimelineCanvas<'_> {
    fn submit(&self) {
        self.rect_callback.set_instances(self.rect_scratch);
        self.texture_callback.set_instances(self.texture_scratch);
    }
}

fn straight_linear_color(color: egui::Color32) -> [f32; 4] {
    let [red, green, blue, alpha] = egui::Rgba::from(color).to_array();
    if alpha > 0.0 {
        [red / alpha, green / alpha, blue / alpha, alpha]
    } else {
        [0.0; 4]
    }
}

impl TimelineCanvas for NativeTimelineCanvas<'_> {
    fn begin(&mut self, ui: &mut egui::Ui, canvas_rect: egui::Rect) {
        self.rect_scratch.clear();
        self.texture_scratch.clear();
        self.rect_callback.install(ui.painter(), canvas_rect);
        self.texture_callback.install(ui.painter(), canvas_rect);
    }

    fn solid_rect(&mut self, rect: egui::Rect, color: egui::Color32) {
        if !rect.is_positive() {
            return;
        }
        self.rect_scratch.push(RectInstance::new(
            [rect.left(), rect.top(), rect.width(), rect.height()],
            straight_linear_color(color),
        ));
    }

    fn texture_rect(
        &mut self,
        rect: egui::Rect,
        native_texture_id: u64,
        _fallback_texture: egui::TextureId,
        uv: egui::Rect,
        tint: egui::Color32,
    ) {
        if !rect.is_positive() || !uv.is_positive() {
            return;
        }
        self.texture_scratch.push(TexturedRect::new(
            native_texture_id,
            TextureInstance::new(
                [rect.left(), rect.top(), rect.width(), rect.height()],
                [uv.left(), uv.top(), uv.right(), uv.bottom()],
                straight_linear_color(tint),
            ),
        ));
    }
}

/// Bridges shared compositor geometry to one retained, double-buffered native viewer callback.
struct NativeViewerCanvas {
    callback: ViewerCompositorCallbackHandle,
    logical_canvas_rect: egui::Rect,
    project_size: Option<nle_compositor::PixelSize>,
    layers: [Option<ViewerLayerPrimitive>; MONITOR_LAYER_COUNT],
    black_mattes_before: [f32; MONITOR_LAYER_COUNT + 1],
    white_mattes_before: [f32; MONITOR_LAYER_COUNT + 1],
    submitted_layers: usize,
}

impl NativeViewerCanvas {
    fn new(callback: ViewerCompositorCallbackHandle) -> Self {
        Self {
            callback,
            logical_canvas_rect: egui::Rect::NOTHING,
            project_size: None,
            layers: [None; MONITOR_LAYER_COUNT],
            black_mattes_before: [0.0; MONITOR_LAYER_COUNT + 1],
            white_mattes_before: [0.0; MONITOR_LAYER_COUNT + 1],
            submitted_layers: 0,
        }
    }

    fn submit(&self) {
        let Some(project_size) = self.project_size else {
            self.callback.clear();
            return;
        };
        self.callback.set_frame(ViewerFrame {
            project_size,
            logical_canvas_rect: self.logical_canvas_rect,
            layers: self.layers,
            black_mattes_before: self.black_mattes_before,
            white_mattes_before: self.white_mattes_before,
        });
    }
}

impl ViewerCanvas for NativeViewerCanvas {
    fn begin(
        &mut self,
        ui: &mut egui::Ui,
        canvas_rect: egui::Rect,
        project_size: nle_compositor::PixelSize,
    ) {
        self.logical_canvas_rect = canvas_rect;
        self.project_size = Some(project_size);
        self.layers = [None; MONITOR_LAYER_COUNT];
        self.black_mattes_before = [0.0; MONITOR_LAYER_COUNT + 1];
        self.white_mattes_before = [0.0; MONITOR_LAYER_COUNT + 1];
        self.submitted_layers = 0;
        self.callback.install(ui.painter(), canvas_rect);
    }

    fn layer(
        &mut self,
        layer: usize,
        _frame: MonitorFrame,
        content_uv: egui::Rect,
        quad: nle_compositor::CompositeQuad,
        effects: nle_timeline::EvaluatedVideoEffectStack,
    ) {
        let Some(slot) = self.layers.get_mut(layer) else {
            return;
        };
        debug_assert_eq!(
            nle_timeline::MAX_VIDEO_EFFECTS_PER_CLIP,
            MAX_COLOR_CORRECTIONS_PER_LAYER
        );
        let mut color_corrections =
            [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER];
        for (destination, effect) in color_corrections.iter_mut().zip(effects.active()) {
            *destination = viewer_color_correction(*effect);
        }
        *slot = Some(ViewerLayerPrimitive {
            quad,
            content_uv: [
                nle_compositor::Uv {
                    u: content_uv.left(),
                    v: content_uv.top(),
                },
                nle_compositor::Uv {
                    u: content_uv.right(),
                    v: content_uv.top(),
                },
                nle_compositor::Uv {
                    u: content_uv.right(),
                    v: content_uv.bottom(),
                },
                nle_compositor::Uv {
                    u: content_uv.left(),
                    v: content_uv.bottom(),
                },
            ],
            color_corrections,
            color_correction_count: effects.len() as u32,
        });
        self.submitted_layers = self.submitted_layers.max(layer.saturating_add(1));
    }

    fn black_matte(&mut self, opacity: f32) {
        let boundary = self.submitted_layers.min(MONITOR_LAYER_COUNT);
        let opacity = opacity.clamp(0.0, 1.0);
        let current = self.black_mattes_before[boundary];
        self.black_mattes_before[boundary] = 1.0 - (1.0 - current) * (1.0 - opacity);
    }

    fn white_matte(&mut self, opacity: f32) {
        let boundary = self.submitted_layers.min(MONITOR_LAYER_COUNT);
        let opacity = opacity.clamp(0.0, 1.0);
        let current = self.white_mattes_before[boundary];
        self.white_mattes_before[boundary] = 1.0 - (1.0 - current) * (1.0 - opacity);
    }
}

fn viewer_color_correction(effect: nle_timeline::EvaluatedVideoEffect) -> ViewerColorCorrection {
    let viewer_curve = |curve: nle_timeline::EvaluatedColorCurve| ViewerColorCurve {
        points: curve.points.map(|point| [point.x, point.y]),
        count: u32::from(curve.count),
    };
    match effect {
        nle_timeline::EvaluatedVideoEffect::BrightnessContrast(correction) => {
            ViewerColorCorrection {
                temperature: correction.temperature,
                tint: correction.tint,
                saturation: correction.saturation,
                exposure: correction.exposure,
                brightness: correction.brightness,
                contrast: correction.contrast,
                highlights: correction.highlights,
                shadows: correction.shadows,
                whites: correction.whites,
                blacks: correction.blacks,
                curves: ViewerRgbCurves {
                    master: viewer_curve(correction.curves.master),
                    red: viewer_curve(correction.curves.red),
                    green: viewer_curve(correction.curves.green),
                    blue: viewer_curve(correction.curves.blue),
                },
                ..Default::default()
            }
        }
        nle_timeline::EvaluatedVideoEffect::Vignette(vignette) => {
            // A vignette must not alter curves; the constructor keeps the default identity LUT.
            ViewerColorCorrection::vignette(
                vignette.amount,
                vignette.midpoint,
                vignette.feather,
                vignette.center_x,
                vignette.center_y,
            )
        }
    }
}

const FRAME_TIME_SAMPLE_COUNT: usize = 120;
const FRAME_TIME_PUBLISH_INTERVAL: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq)]
struct FramePerformance {
    latest_ms: f32,
    p95_ms: f32,
    native_rects: usize,
    native_textures: usize,
}

struct FrameMetrics {
    samples_ms: [f32; FRAME_TIME_SAMPLE_COUNT],
    sample_count: usize,
    next_sample: usize,
    frames_since_publish: usize,
}

impl Default for FrameMetrics {
    fn default() -> Self {
        Self {
            samples_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            sample_count: 0,
            next_sample: 0,
            frames_since_publish: FRAME_TIME_PUBLISH_INTERVAL - 1,
        }
    }
}

impl FrameMetrics {
    fn record(
        &mut self,
        duration: Duration,
        native_rects: usize,
        native_textures: usize,
    ) -> Option<FramePerformance> {
        let latest_ms = duration.as_secs_f32() * 1_000.0;
        self.samples_ms[self.next_sample] = latest_ms;
        self.next_sample = (self.next_sample + 1) % FRAME_TIME_SAMPLE_COUNT;
        self.sample_count = (self.sample_count + 1).min(FRAME_TIME_SAMPLE_COUNT);
        self.frames_since_publish += 1;
        if self.frames_since_publish < FRAME_TIME_PUBLISH_INTERVAL {
            return None;
        }
        self.frames_since_publish = 0;
        let mut ordered = self.samples_ms;
        ordered[..self.sample_count].sort_unstable_by(f32::total_cmp);
        let p95_index = self
            .sample_count
            .saturating_mul(95)
            .div_ceil(100)
            .saturating_sub(1);
        Some(FramePerformance {
            latest_ms,
            p95_ms: ordered[p95_index],
            native_rects,
            native_textures,
        })
    }
}

struct MonitorRuntimeMetrics {
    requests: u64,
    completed_frames: u64,
    presented_frames: u64,
    dropped_frames: u64,
    hold_events: u64,
    late_frames: u64,
    errors: u64,
    native_uploads: u64,
    fallback_uploads: u64,
    turnaround_ms: [f32; FRAME_TIME_SAMPLE_COUNT],
    turnaround_count: usize,
    next_turnaround: usize,
}

impl Default for MonitorRuntimeMetrics {
    fn default() -> Self {
        Self {
            requests: 0,
            completed_frames: 0,
            presented_frames: 0,
            dropped_frames: 0,
            hold_events: 0,
            late_frames: 0,
            errors: 0,
            native_uploads: 0,
            fallback_uploads: 0,
            turnaround_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            turnaround_count: 0,
            next_turnaround: 0,
        }
    }
}

impl MonitorRuntimeMetrics {
    fn record_request(&mut self) {
        self.requests = self.requests.saturating_add(1);
    }

    fn record_completed(
        &mut self,
        turnaround: Option<Duration>,
        frame_budget_ms: f32,
        retained_previous_frame: bool,
    ) {
        self.completed_frames = self.completed_frames.saturating_add(1);
        let Some(turnaround) = turnaround else {
            return;
        };
        let milliseconds = turnaround.as_secs_f32() * 1_000.0;
        self.turnaround_ms[self.next_turnaround] = milliseconds;
        self.next_turnaround = (self.next_turnaround + 1) % FRAME_TIME_SAMPLE_COUNT;
        self.turnaround_count = (self.turnaround_count + 1).min(FRAME_TIME_SAMPLE_COUNT);
        if milliseconds > frame_budget_ms.max(0.0) {
            self.late_frames = self.late_frames.saturating_add(1);
            if retained_previous_frame {
                self.hold_events = self.hold_events.saturating_add(1);
            }
        }
    }

    fn record_presented(&mut self, native_upload: bool) {
        self.presented_frames = self.presented_frames.saturating_add(1);
        if native_upload {
            self.native_uploads = self.native_uploads.saturating_add(1);
        } else {
            self.fallback_uploads = self.fallback_uploads.saturating_add(1);
        }
    }

    fn record_dropped(&mut self) {
        self.dropped_frames = self.dropped_frames.saturating_add(1);
    }

    fn record_error(&mut self) {
        self.errors = self.errors.saturating_add(1);
    }

    fn diagnostics(
        &self,
        audio_underrun_frames: u64,
        audio_callback_lock_failures: u64,
        audio_late_discarded_frames: u64,
    ) -> RuntimeDiagnostics {
        let mut ordered = self.turnaround_ms;
        ordered[..self.turnaround_count].sort_unstable_by(f32::total_cmp);
        let p95_index = self
            .turnaround_count
            .saturating_mul(95)
            .div_ceil(100)
            .saturating_sub(1);
        RuntimeDiagnostics {
            monitor_requests: self.requests,
            monitor_completed_frames: self.completed_frames,
            monitor_presented_frames: self.presented_frames,
            monitor_dropped_frames: self.dropped_frames,
            monitor_hold_events: self.hold_events,
            monitor_late_frames: self.late_frames,
            monitor_errors: self.errors,
            monitor_turnaround_p95_ms: ordered.get(p95_index).copied().unwrap_or_default(),
            native_viewer_uploads: self.native_uploads,
            fallback_viewer_uploads: self.fallback_uploads,
            audio_underrun_frames,
            audio_callback_lock_failures,
            audio_late_discarded_frames,
            live_pipeline_timing: LivePipelineTiming::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SurfaceSubmissionMetrics {
    samples: usize,
    cpu_p95_ms: f32,
    surface_submission_interval_p95_ms: f32,
    surface_present_call_cpu_p95_ms: f32,
    average_submission_fps: f32,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
struct SurfaceSubmissionReport {
    schema_version: u32,
    samples: usize,
    cpu_p95_ms: f32,
    surface_submission_interval_p95_ms: f32,
    surface_present_call_cpu_p95_ms: f32,
    average_submission_fps: f32,
    renderer_gpu_name: String,
    renderer_vendor_id: u32,
    renderer_device_id: u32,
    renderer_device_type: String,
    renderer_backend: String,
    renderer_driver: String,
    renderer_driver_info: String,
    decoder_backends: Vec<String>,
    encoder_backend: String,
    cpu_identity: Option<String>,
    logical_cpu_count: usize,
    total_physical_memory_bytes: Option<u64>,
    selected_preview_quality: String,
    resolved_preview_quality: String,
    preview_width: u32,
    preview_height: u32,
    monitor_cache_cap_bytes: usize,
    display_refresh_millihertz: Option<u32>,
    decoder_stage_timings: DecoderStageTimingsReport,
    viewer_stage_timings: ViewerStageTimingsReport,
    gpu_stage_timings: GpuStageTimingsReport,
    audio_stage_timings: AudioStageTimingsReport,
    runtime_diagnostics: RuntimeDiagnosticsReport,
}

/// CPU/API submission timing only; it does not measure GPU completion or scanout.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct ViewerStageTimingReport {
    samples: usize,
    p95_ms: f32,
    max_ms: f32,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct ViewerStageTimingsReport {
    upload_cpu: ViewerStageTimingReport,
    compositor_encode_cpu: ViewerStageTimingReport,
}

/// GPU timing availability plus isolated compositor-pass and whole-submission observations.
/// Neither metric is presentation, DWM, or physical scanout timing.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct GpuStageTimingsReport {
    timestamp_query_supported: bool,
    composite_pass_gpu: Option<ViewerStageTimingReport>,
    submission_to_completion_elapsed: ViewerStageTimingReport,
}

impl From<nle_render::ViewerCompositorEncodeTiming> for ViewerStageTimingReport {
    fn from(timing: nle_render::ViewerCompositorEncodeTiming) -> Self {
        Self {
            samples: timing.samples,
            p95_ms: timing.p95_ms,
            max_ms: timing.max_ms,
        }
    }
}

impl From<nle_render::GpuSubmissionCompletionTiming> for ViewerStageTimingReport {
    fn from(timing: nle_render::GpuSubmissionCompletionTiming) -> Self {
        Self {
            samples: timing.samples,
            p95_ms: timing.p95_ms,
            max_ms: timing.max_ms,
        }
    }
}

impl From<nle_render::ViewerCompositorGpuTiming> for ViewerStageTimingReport {
    fn from(timing: nle_render::ViewerCompositorGpuTiming) -> Self {
        Self {
            samples: timing.samples,
            p95_ms: timing.p95_ms,
            max_ms: timing.max_ms,
        }
    }
}

impl GpuStageTimingsReport {
    fn from_snapshots(
        composite: nle_render::ViewerCompositorGpuTiming,
        submission: nle_render::GpuSubmissionCompletionTiming,
    ) -> Self {
        Self {
            timestamp_query_supported: composite.supported,
            composite_pass_gpu: composite.supported.then_some(composite.into()),
            submission_to_completion_elapsed: submission.into(),
        }
    }

    fn fully_observed(&self) -> bool {
        self.submission_to_completion_elapsed.samples > 0
            && (!self.timestamp_query_supported
                || self
                    .composite_pass_gpu
                    .is_some_and(|timing| timing.samples > 0))
    }
}

struct ViewerStageTimingWindow {
    samples_ms: [f32; FRAME_TIME_SAMPLE_COUNT],
    sample_count: usize,
    next_sample: usize,
}

impl Default for ViewerStageTimingWindow {
    fn default() -> Self {
        Self {
            samples_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            sample_count: 0,
            next_sample: 0,
        }
    }
}

impl ViewerStageTimingWindow {
    fn record(&mut self, duration: Duration) {
        self.samples_ms[self.next_sample] = duration.as_secs_f32() * 1_000.0;
        self.next_sample = (self.next_sample + 1) % FRAME_TIME_SAMPLE_COUNT;
        self.sample_count = (self.sample_count + 1).min(FRAME_TIME_SAMPLE_COUNT);
    }

    fn snapshot(&self) -> ViewerStageTimingReport {
        let mut ordered = self.samples_ms;
        ordered[..self.sample_count].sort_unstable_by(f32::total_cmp);
        let p95_index = self
            .sample_count
            .saturating_mul(95)
            .div_ceil(100)
            .saturating_sub(1);
        ViewerStageTimingReport {
            samples: self.sample_count,
            p95_ms: ordered.get(p95_index).copied().unwrap_or_default(),
            max_ms: ordered
                .get(self.sample_count.saturating_sub(1))
                .copied()
                .unwrap_or_default(),
        }
    }
}

fn live_mean_stage_sample(timing: DecoderStageTimingReport) -> Option<LivePipelineTimingSample> {
    (timing.samples > 0).then_some(LivePipelineTimingSample {
        representative: LivePipelineTimingRepresentative::Mean,
        representative_ms: timing.mean_ms as f32,
        max_ms: timing.max_ms as f32,
        samples: timing.samples,
    })
}

fn live_p95_stage_sample(
    samples: usize,
    p95_ms: f32,
    max_ms: f32,
) -> Option<LivePipelineTimingSample> {
    (samples > 0).then_some(LivePipelineTimingSample {
        representative: LivePipelineTimingRepresentative::P95,
        representative_ms: p95_ms,
        max_ms,
        samples: u64::try_from(samples).unwrap_or(u64::MAX),
    })
}

fn live_audio_mean_stage_sample(
    timing: nle_audio::AudioCallbackCpuTiming,
) -> Option<LivePipelineTimingSample> {
    if timing.samples == 0 {
        return None;
    }
    Some(LivePipelineTimingSample {
        representative: LivePipelineTimingRepresentative::Mean,
        representative_ms: (timing.total_nanos as f64 / timing.samples as f64 / 1_000_000.0) as f32,
        max_ms: timing.max_nanos as f32 / 1_000_000.0,
        samples: timing.samples,
    })
}

impl ViewerStageTimingsReport {
    fn fully_observed(&self) -> bool {
        self.upload_cpu.samples > 0 && self.compositor_encode_cpu.samples > 0
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct DecoderStageTimingReport {
    samples: u64,
    total_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

impl From<nle_decode::MonitorStageTiming> for DecoderStageTimingReport {
    fn from(timing: nle_decode::MonitorStageTiming) -> Self {
        Self {
            samples: timing.samples,
            total_ms: timing.total_ms(),
            mean_ms: timing.mean_ms(),
            max_ms: timing.max_ms(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
struct DecoderStageTimingsReport {
    cache_lookup: DecoderStageTimingReport,
    demux_packet: DecoderStageTimingReport,
    decoder_calls: DecoderStageTimingReport,
    hardware_transfer: DecoderStageTimingReport,
    scaler: DecoderStageTimingReport,
    rgba_copy_letterbox: DecoderStageTimingReport,
    worker_request: DecoderStageTimingReport,
}

impl From<nle_decode::MonitorDecoderStageTimings> for DecoderStageTimingsReport {
    fn from(timings: nle_decode::MonitorDecoderStageTimings) -> Self {
        Self {
            cache_lookup: timings.cache_lookup.into(),
            demux_packet: timings.demux_packet.into(),
            decoder_calls: timings.decoder_calls.into(),
            hardware_transfer: timings.hardware_transfer.into(),
            scaler: timings.scaler.into(),
            rgba_copy_letterbox: timings.rgba_copy_letterbox.into(),
            worker_request: timings.worker_request.into(),
        }
    }
}

impl DecoderStageTimingsReport {
    fn applicable_decode_stages_observed(&self) -> bool {
        [
            self.cache_lookup,
            self.demux_packet,
            self.decoder_calls,
            self.scaler,
            self.rgba_copy_letterbox,
            self.worker_request,
        ]
        .into_iter()
        .all(|stage| stage.samples > 0)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct AudioStageTimingReport {
    samples: u64,
    total_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

impl From<nle_audio::AudioCallbackCpuTiming> for AudioStageTimingReport {
    fn from(timing: nle_audio::AudioCallbackCpuTiming) -> Self {
        let total_ms = timing.total_nanos as f64 / 1_000_000.0;
        Self {
            samples: timing.samples,
            total_ms,
            mean_ms: if timing.samples == 0 {
                0.0
            } else {
                total_ms / timing.samples as f64
            },
            max_ms: timing.max_nanos as f64 / 1_000_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct AudioStageTimingsReport {
    output_callback_cpu: AudioStageTimingReport,
    mix_render_cpu: AudioStageTimingReport,
}

impl AudioStageTimingsReport {
    fn fully_observed(&self) -> bool {
        self.output_callback_cpu.samples > 0 && self.mix_render_cpu.samples > 0
    }
}

#[derive(Clone, Debug)]
struct RendererReport {
    name: String,
    vendor_id: u32,
    device_id: u32,
    device_type: String,
    backend: String,
    driver: String,
    driver_info: String,
}

#[derive(Clone, Debug)]
struct SurfaceReportEnvironment {
    renderer: RendererReport,
    decoder_backends: Vec<String>,
    encoder_backend: String,
    machine: MachineProfile,
    selected_preview_quality: String,
    resolved_preview_quality: String,
    preview_size: [u32; 2],
    monitor_cache_cap_bytes: usize,
    display_refresh_millihertz: Option<u32>,
    decoder_stage_timings: DecoderStageTimingsReport,
    viewer_stage_timings: ViewerStageTimingsReport,
    gpu_stage_timings: GpuStageTimingsReport,
    audio_stage_timings: AudioStageTimingsReport,
    runtime_diagnostics: RuntimeDiagnosticsReport,
}

fn surface_report_backends_ready(
    full_media_smoke: bool,
    decoder_backends: &[String],
    encoder_backend: Option<&str>,
) -> bool {
    !full_media_smoke || (!decoder_backends.is_empty() && encoder_backend.is_some())
}

fn surface_report_stage_timings_ready(
    full_media_smoke: bool,
    timings: &DecoderStageTimingsReport,
) -> bool {
    !full_media_smoke || timings.applicable_decode_stages_observed()
}

fn surface_report_viewer_stage_timings_ready(
    full_media_smoke: bool,
    timings: ViewerStageTimingsReport,
) -> bool {
    !full_media_smoke || timings.fully_observed()
}

fn surface_report_gpu_stage_timings_ready(
    full_media_smoke: bool,
    timings: GpuStageTimingsReport,
) -> bool {
    !full_media_smoke || timings.fully_observed()
}

fn surface_report_audio_stage_timings_ready(
    full_media_smoke: bool,
    timings: AudioStageTimingsReport,
) -> bool {
    !full_media_smoke || timings.fully_observed()
}

fn aggregate_monitor_decoder_stage_timings(
    decoders: &[nle_decode::MonitorDecoder],
) -> DecoderStageTimingsReport {
    let mut aggregate = nle_decode::MonitorDecoderStageTimings::default();
    for decoder in decoders {
        aggregate.merge(decoder.stage_timings());
    }
    aggregate.into()
}

#[derive(Clone, Copy)]
struct StartupPresentationReport {
    first_surface_present_ms: f32,
}

struct StartupPresentationProbe {
    started_at: Instant,
    report_tx: Option<mpsc::SyncSender<StartupPresentationReport>>,
}

struct SurfaceSubmissionProbe {
    cpu_ms: [f32; FRAME_TIME_SAMPLE_COUNT],
    intervals_ms: [f32; FRAME_TIME_SAMPLE_COUNT],
    present_call_ms: [f32; FRAME_TIME_SAMPLE_COUNT],
    sample_count: usize,
    last_submitted_at: Option<Instant>,
    completed: Option<SurfaceSubmissionMetrics>,
    report_tx: Option<mpsc::SyncSender<SurfaceSubmissionReport>>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
struct RuntimeDiagnosticsReport {
    monitor_requests: u64,
    monitor_completed_frames: u64,
    monitor_presented_frames: u64,
    monitor_dropped_frames: u64,
    monitor_hold_events: u64,
    monitor_late_frames: u64,
    monitor_errors: u64,
    /// Rolling end-to-end request turnaround p95 at the time this snapshot was taken.
    monitor_turnaround_window_p95_ms: f32,
    native_viewer_uploads: u64,
    fallback_viewer_uploads: u64,
    audio_underrun_frames: u64,
    audio_callback_lock_failures: u64,
    audio_late_discarded_frames: u64,
}

impl RuntimeDiagnosticsReport {
    fn delta_since(self, baseline: Self) -> Self {
        Self {
            monitor_requests: self
                .monitor_requests
                .saturating_sub(baseline.monitor_requests),
            monitor_completed_frames: self
                .monitor_completed_frames
                .saturating_sub(baseline.monitor_completed_frames),
            monitor_presented_frames: self
                .monitor_presented_frames
                .saturating_sub(baseline.monitor_presented_frames),
            monitor_dropped_frames: self
                .monitor_dropped_frames
                .saturating_sub(baseline.monitor_dropped_frames),
            monitor_hold_events: self
                .monitor_hold_events
                .saturating_sub(baseline.monitor_hold_events),
            monitor_late_frames: self
                .monitor_late_frames
                .saturating_sub(baseline.monitor_late_frames),
            monitor_errors: self.monitor_errors.saturating_sub(baseline.monitor_errors),
            // A percentile is not additive. Preserve the final rolling window instead of
            // subtracting two unrelated distributions.
            monitor_turnaround_window_p95_ms: self.monitor_turnaround_window_p95_ms,
            native_viewer_uploads: self
                .native_viewer_uploads
                .saturating_sub(baseline.native_viewer_uploads),
            fallback_viewer_uploads: self
                .fallback_viewer_uploads
                .saturating_sub(baseline.fallback_viewer_uploads),
            audio_underrun_frames: self
                .audio_underrun_frames
                .saturating_sub(baseline.audio_underrun_frames),
            audio_callback_lock_failures: self
                .audio_callback_lock_failures
                .saturating_sub(baseline.audio_callback_lock_failures),
            audio_late_discarded_frames: self
                .audio_late_discarded_frames
                .saturating_sub(baseline.audio_late_discarded_frames),
        }
    }
}

impl From<RuntimeDiagnostics> for RuntimeDiagnosticsReport {
    fn from(diagnostics: RuntimeDiagnostics) -> Self {
        Self {
            monitor_requests: diagnostics.monitor_requests,
            monitor_completed_frames: diagnostics.monitor_completed_frames,
            monitor_presented_frames: diagnostics.monitor_presented_frames,
            monitor_dropped_frames: diagnostics.monitor_dropped_frames,
            monitor_hold_events: diagnostics.monitor_hold_events,
            monitor_late_frames: diagnostics.monitor_late_frames,
            monitor_errors: diagnostics.monitor_errors,
            monitor_turnaround_window_p95_ms: diagnostics.monitor_turnaround_p95_ms,
            native_viewer_uploads: diagnostics.native_viewer_uploads,
            fallback_viewer_uploads: diagnostics.fallback_viewer_uploads,
            audio_underrun_frames: diagnostics.audio_underrun_frames,
            audio_callback_lock_failures: diagnostics.audio_callback_lock_failures,
            audio_late_discarded_frames: diagnostics.audio_late_discarded_frames,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
struct PlaybackSoakMonitorResources {
    frame_cache_capacity_bytes: usize,
    current_frame_cache_bytes: usize,
    /// Exact historical peak from the single app-wide decoded-frame cache.
    peak_frame_cache_bytes_upper_bound: usize,
    active_sticky_sessions: usize,
    /// Exact historical peak from the single app-wide permit pool.
    peak_sticky_sessions: usize,
    session_cap: usize,
    active_foreground_sessions: usize,
    foreground_session_cap: usize,
    active_background_sessions: usize,
    background_session_cap: usize,
    live_source_groups: usize,
    source_group_cap: usize,
    live_lane_actors: usize,
    lane_actor_cap: usize,
    retiring_lane_actors: usize,
}

fn aggregate_playback_soak_monitor_resources(
    frame_cache_pool: &nle_decode::MonitorFrameCachePool,
    session_pool: nle_decode::MonitorSessionPoolDiagnostics,
    source_coordinator: nle_decode::MonitorSourceCoordinatorDiagnostics,
) -> PlaybackSoakMonitorResources {
    aggregate_playback_soak_monitor_resource_diagnostics(
        frame_cache_pool.diagnostics(),
        session_pool,
        source_coordinator,
    )
}

fn aggregate_playback_soak_monitor_resource_diagnostics(
    frame_cache: nle_decode::MonitorFrameCachePoolDiagnostics,
    session_pool: nle_decode::MonitorSessionPoolDiagnostics,
    source_coordinator: nle_decode::MonitorSourceCoordinatorDiagnostics,
) -> PlaybackSoakMonitorResources {
    let mut resources = PlaybackSoakMonitorResources {
        frame_cache_capacity_bytes: frame_cache.capacity_bytes,
        current_frame_cache_bytes: frame_cache.current_bytes,
        peak_frame_cache_bytes_upper_bound: frame_cache.peak_bytes,
        active_sticky_sessions: 0,
        peak_sticky_sessions: session_pool.peak_sticky_sessions,
        session_cap: session_pool.session_cap,
        active_foreground_sessions: session_pool.active_foreground_sessions,
        foreground_session_cap: session_pool.foreground_session_cap,
        active_background_sessions: session_pool.active_background_sessions,
        background_session_cap: session_pool.background_session_cap,
        live_source_groups: source_coordinator.live_source_groups,
        source_group_cap: source_coordinator.source_group_cap,
        live_lane_actors: source_coordinator.live_lane_actors,
        lane_actor_cap: source_coordinator.lane_actor_cap,
        retiring_lane_actors: source_coordinator.retiring_lane_actors,
    };
    resources.active_sticky_sessions = session_pool.active_sticky_sessions;
    resources
}

#[derive(Clone, Debug, Serialize, PartialEq)]
struct PlaybackSoakReport {
    schema_version: u32,
    requested_duration_seconds: u64,
    actual_duration_seconds: f64,
    loop_count: u64,
    observed_decoder_backends: Vec<String>,
    selected_preview_quality: String,
    resolved_preview_quality: String,
    monitor_cache_cap_bytes: usize,
    monitor_resources: PlaybackSoakMonitorResources,
    decoder_stage_timings: DecoderStageTimingsReport,
    audio_transport_healthy_at_completion: bool,
    audio_fault_observed: bool,
    unexpected_playback_stop_observed: bool,
    runtime_diagnostics_delta: RuntimeDiagnosticsReport,
}

struct PlaybackSoakProbe {
    requested_duration: Duration,
    started_at: Option<Instant>,
    baseline_diagnostics: Option<RuntimeDiagnosticsReport>,
    loop_count: u64,
    audio_fault_observed: bool,
    unexpected_playback_stop_observed: bool,
    report_tx: Option<mpsc::SyncSender<PlaybackSoakReport>>,
}

fn playback_soak_duration_seconds(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_PLAYBACK_SOAK_SECONDS)
        .clamp(1, MAX_PLAYBACK_SOAK_SECONDS)
}

impl PlaybackSoakProbe {
    /// The soak is deliberately opt-in and only works with the existing real-media drag path.
    /// No normal launch allocates, logs, or otherwise changes playback behavior for this probe.
    fn from_environment() -> Option<Self> {
        std::env::var_os("MAELSTROM_MEDIA_ACCEPTANCE_PATH")?;
        let path = PathBuf::from(std::env::var_os("MAELSTROM_PLAYBACK_SOAK_REPORT")?);
        let requested_duration = Duration::from_secs(playback_soak_duration_seconds(
            std::env::var("MAELSTROM_PLAYBACK_SOAK_SECONDS")
                .ok()
                .as_deref(),
        ));
        let (report_tx, report_rx) = mpsc::sync_channel::<PlaybackSoakReport>(1);
        thread::Builder::new()
            .name("maelstrom-playback-soak-report".into())
            .spawn(move || {
                let Ok(report) = report_rx.recv() else {
                    return;
                };
                let Ok(mut json) = serde_json::to_string_pretty(&report) else {
                    return;
                };
                json.push('\n');
                let _ = write_atomic_report(&path, &json);
            })
            .ok()?;
        Some(Self {
            requested_duration,
            started_at: None,
            baseline_diagnostics: None,
            loop_count: 0,
            audio_fault_observed: false,
            unexpected_playback_stop_observed: false,
            report_tx: Some(report_tx),
        })
    }

    fn start_after_real_playback(&mut self, now: Instant, diagnostics: RuntimeDiagnostics) {
        if self.started_at.is_none() {
            self.started_at = Some(now);
            self.baseline_diagnostics = Some(diagnostics.into());
        }
    }

    fn is_started(&self) -> bool {
        self.started_at.is_some()
    }

    fn record_loop(&mut self) {
        self.loop_count = self.loop_count.saturating_add(1);
    }

    fn observe_transport_state(
        &mut self,
        audio_error_present: bool,
        playing: bool,
        audio_transport_active: bool,
        reached_timeline_end: bool,
    ) {
        if !self.is_started() {
            return;
        }
        self.audio_fault_observed |= audio_error_present || (playing && !audio_transport_active);
        self.unexpected_playback_stop_observed |= !playing && !reached_timeline_end;
    }

    fn due(&self, now: Instant) -> bool {
        self.started_at
            .is_some_and(|started_at| now.duration_since(started_at) >= self.requested_duration)
    }

    // The report owns one explicit field per independently validated soak signal.
    #[allow(clippy::too_many_arguments)]
    fn report(
        &self,
        now: Instant,
        diagnostics: RuntimeDiagnostics,
        observed_decoder_backends: Vec<String>,
        selected_preview_quality: String,
        resolved_preview_quality: String,
        monitor_cache_cap_bytes: usize,
        monitor_resources: PlaybackSoakMonitorResources,
        decoder_stage_timings: DecoderStageTimingsReport,
        audio_transport_healthy_now: bool,
    ) -> Option<PlaybackSoakReport> {
        let started_at = self.started_at?;
        Some(PlaybackSoakReport {
            schema_version: 5,
            requested_duration_seconds: self.requested_duration.as_secs(),
            actual_duration_seconds: now.duration_since(started_at).as_secs_f64(),
            loop_count: self.loop_count,
            observed_decoder_backends,
            selected_preview_quality,
            resolved_preview_quality,
            monitor_cache_cap_bytes,
            monitor_resources,
            decoder_stage_timings,
            audio_transport_healthy_at_completion: audio_transport_healthy_now
                && !self.audio_fault_observed
                && !self.unexpected_playback_stop_observed,
            audio_fault_observed: self.audio_fault_observed,
            unexpected_playback_stop_observed: self.unexpected_playback_stop_observed,
            runtime_diagnostics_delta: RuntimeDiagnosticsReport::from(diagnostics)
                .delta_since(self.baseline_diagnostics.unwrap_or_default()),
        })
    }

    fn publish(&mut self, report: PlaybackSoakReport) -> bool {
        self.report_tx
            .take()
            .is_some_and(|tx| tx.try_send(report).is_ok())
    }
}

fn write_atomic_report(path: &Path, contents: &str) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)
}

impl StartupPresentationProbe {
    fn from_environment(started_at: Instant) -> Option<Self> {
        let path = PathBuf::from(std::env::var_os("MAELSTROM_STARTUP_REPORT")?);
        let (report_tx, report_rx) = mpsc::sync_channel::<StartupPresentationReport>(1);
        thread::Builder::new()
            .name("maelstrom-startup-presentation-report".into())
            .spawn(move || {
                let Ok(report) = report_rx.recv() else {
                    return;
                };
                let json = format!(
                    "{{\n  \"first_surface_present_ms\": {:.4}\n}}\n",
                    report.first_surface_present_ms
                );
                let _ = write_atomic_report(&path, &json);
            })
            .ok()?;
        Some(Self {
            started_at,
            report_tx: Some(report_tx),
        })
    }

    /// Sends one scalar to a dedicated report worker after the first successful present.
    fn record_first_present(&mut self, presented_at: Instant) {
        let Some(tx) = self.report_tx.take() else {
            return;
        };
        let _ = tx.try_send(StartupPresentationReport {
            first_surface_present_ms: presented_at.duration_since(self.started_at).as_secs_f32()
                * 1_000.0,
        });
    }
}

impl SurfaceSubmissionProbe {
    fn from_environment() -> Option<Self> {
        let path = PathBuf::from(std::env::var_os("MAELSTROM_SURFACE_SUBMISSION_REPORT")?);
        let (report_tx, report_rx) = mpsc::sync_channel::<SurfaceSubmissionReport>(1);
        thread::Builder::new()
            .name("maelstrom-surface-submission-report".into())
            .spawn(move || {
                let Ok(report) = report_rx.recv() else {
                    return;
                };
                let Ok(mut json) = serde_json::to_string_pretty(&report) else {
                    return;
                };
                json.push('\n');
                let _ = write_atomic_report(&path, &json);
            })
            .ok()?;
        Some(Self {
            cpu_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            intervals_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            present_call_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            sample_count: 0,
            last_submitted_at: None,
            completed: None,
            report_tx: Some(report_tx),
        })
    }

    /// Records only already-completed CPU work and present submissions. Report IO belongs to a
    /// dedicated one-shot worker, never the UI frame.
    fn record(
        &mut self,
        cpu_duration: Duration,
        present_call_duration: Duration,
        submitted_at: Instant,
    ) -> bool {
        if self.completed.is_some() {
            return false;
        }
        let Some(previous) = self.last_submitted_at.replace(submitted_at) else {
            return true;
        };
        if self.sample_count >= FRAME_TIME_SAMPLE_COUNT {
            return false;
        }
        self.cpu_ms[self.sample_count] = cpu_duration.as_secs_f32() * 1_000.0;
        self.intervals_ms[self.sample_count] =
            submitted_at.duration_since(previous).as_secs_f32() * 1_000.0;
        self.present_call_ms[self.sample_count] = present_call_duration.as_secs_f32() * 1_000.0;
        self.sample_count += 1;
        if self.sample_count < FRAME_TIME_SAMPLE_COUNT {
            return true;
        }

        let cpu_p95_ms = nearest_rank_p95(&self.cpu_ms);
        let surface_submission_interval_p95_ms = nearest_rank_p95(&self.intervals_ms);
        let surface_present_call_cpu_p95_ms = nearest_rank_p95(&self.present_call_ms);
        let mean_interval_ms = self.intervals_ms.iter().sum::<f32>() / self.sample_count as f32;
        self.completed = Some(SurfaceSubmissionMetrics {
            samples: self.sample_count,
            cpu_p95_ms,
            surface_submission_interval_p95_ms,
            surface_present_call_cpu_p95_ms,
            average_submission_fps: 1_000.0 / mean_interval_ms.max(f32::EPSILON),
        });
        false
    }

    fn publish(&mut self, environment: SurfaceReportEnvironment) -> bool {
        let Some(metrics) = self.completed.take() else {
            return false;
        };
        let Some(tx) = self.report_tx.take() else {
            return false;
        };
        let report = SurfaceSubmissionReport {
            schema_version: 7,
            samples: metrics.samples,
            cpu_p95_ms: metrics.cpu_p95_ms,
            surface_submission_interval_p95_ms: metrics.surface_submission_interval_p95_ms,
            surface_present_call_cpu_p95_ms: metrics.surface_present_call_cpu_p95_ms,
            average_submission_fps: metrics.average_submission_fps,
            renderer_gpu_name: environment.renderer.name,
            renderer_vendor_id: environment.renderer.vendor_id,
            renderer_device_id: environment.renderer.device_id,
            renderer_device_type: environment.renderer.device_type,
            renderer_backend: environment.renderer.backend,
            renderer_driver: environment.renderer.driver,
            renderer_driver_info: environment.renderer.driver_info,
            decoder_backends: environment.decoder_backends,
            encoder_backend: environment.encoder_backend,
            cpu_identity: environment.machine.cpu_identity,
            logical_cpu_count: environment.machine.logical_cpu_count,
            total_physical_memory_bytes: environment.machine.total_physical_memory_bytes,
            selected_preview_quality: environment.selected_preview_quality,
            resolved_preview_quality: environment.resolved_preview_quality,
            preview_width: environment.preview_size[0],
            preview_height: environment.preview_size[1],
            monitor_cache_cap_bytes: environment.monitor_cache_cap_bytes,
            display_refresh_millihertz: environment.display_refresh_millihertz,
            decoder_stage_timings: environment.decoder_stage_timings,
            viewer_stage_timings: environment.viewer_stage_timings,
            gpu_stage_timings: environment.gpu_stage_timings,
            audio_stage_timings: environment.audio_stage_timings,
            runtime_diagnostics: environment.runtime_diagnostics,
        };
        tx.try_send(report).is_ok()
    }
}

#[derive(Clone, Copy)]
struct MediaAcceptanceReport {
    media_pool_drag_completed: bool,
    viewer_panel_height: f32,
    timeline_panel_height: f32,
    timeline_view_span_ticks: i64,
    timeline_end_ticks: i64,
    linked_video_bars: usize,
    linked_audio_bars: usize,
    analysis_metadata_ready: bool,
    waveform_ready: bool,
    waveform_peak_count: usize,
    monitor_frame_arrived: bool,
    native_viewer_uploaded: bool,
    playhead_advanced_ticks: i64,
    live_audio_meter_nonzero: bool,
    live_fade_reduced: bool,
    live_fade_recovered: bool,
    live_gain_reduced: bool,
    export_started: bool,
    export_progress_received: bool,
    playhead_advanced_while_exporting: bool,
    export_cancelled: bool,
}

#[derive(Clone, Copy)]
struct MediaAcceptanceInitialEvidence {
    media_pool_drag_completed: bool,
    viewer_panel_height: f32,
    timeline_panel_height: f32,
    timeline_view_span_ticks: i64,
    timeline_end_ticks: i64,
    linked_video_bars: usize,
    linked_audio_bars: usize,
}

struct MediaAcceptanceProbe {
    media_id: u32,
    media_pool_drag_completed: bool,
    viewer_panel_height: f32,
    timeline_panel_height: f32,
    timeline_view_span_ticks: i64,
    timeline_end_ticks: i64,
    linked_video_bars: usize,
    linked_audio_bars: usize,
    analysis_metadata_ready: bool,
    waveform_ready: bool,
    waveform_peak_count: usize,
    monitor_frame_arrived: bool,
    native_viewer_uploaded: bool,
    playback_start_tick: i64,
    playhead_advanced_ticks: i64,
    live_audio_meter_nonzero: bool,
    pre_gain_meter_peak: f32,
    fade_reduction_requested_at_tick: Option<i64>,
    live_fade_reduced: bool,
    fade_clear_requested_at_tick: Option<i64>,
    live_fade_recovered: bool,
    gain_reduction_requested_at_tick: Option<i64>,
    live_gain_reduced: bool,
    export_started: bool,
    export_progress_received: bool,
    playhead_advanced_while_exporting: bool,
    export_cancel_requested: bool,
    export_cancelled: bool,
    report_tx: Option<mpsc::SyncSender<MediaAcceptanceReport>>,
}

#[derive(Clone, Copy)]
enum MediaAcceptanceAudioAction {
    ApplyFade,
    ClearFade,
    ReduceGain,
}

fn editor_layout_is_balanced(viewer_panel_height: f32, timeline_panel_height: f32) -> bool {
    viewer_panel_height.is_finite()
        && viewer_panel_height > 0.0
        && timeline_panel_height.is_finite()
        && timeline_panel_height > 0.0
        && timeline_panel_height > viewer_panel_height * 0.5
        && timeline_panel_height < viewer_panel_height * 1.5
}

fn timeline_view_fits_content(view_span_ticks: i64, timeline_end_ticks: i64) -> bool {
    timeline_end_ticks > 0
        && view_span_ticks >= timeline_end_ticks
        && view_span_ticks <= timeline_end_ticks.saturating_mul(2)
}

impl MediaAcceptanceProbe {
    fn from_environment(media_id: u32, initial: MediaAcceptanceInitialEvidence) -> Option<Self> {
        let path = PathBuf::from(std::env::var_os("MAELSTROM_MEDIA_ACCEPTANCE_REPORT")?);
        let (report_tx, report_rx) = mpsc::sync_channel::<MediaAcceptanceReport>(1);
        thread::Builder::new()
            .name("maelstrom-media-acceptance-report".into())
            .spawn(move || {
                let Ok(report) = report_rx.recv() else {
                    return;
                };
                let json = format!(
                    concat!(
                        "{{\n",
                        "  \"media_pool_drag_completed\": {},\n",
                        "  \"viewer_panel_height\": {:.2},\n",
                        "  \"timeline_panel_height\": {:.2},\n",
                        "  \"timeline_view_span_ticks\": {},\n",
                        "  \"timeline_end_ticks\": {},\n",
                        "  \"linked_video_bars\": {},\n",
                        "  \"linked_audio_bars\": {},\n",
                        "  \"analysis_metadata_ready\": {},\n",
                        "  \"waveform_ready\": {},\n",
                        "  \"waveform_peak_count\": {},\n",
                        "  \"monitor_frame_arrived\": {},\n",
                        "  \"native_viewer_uploaded\": {},\n",
                        "  \"playhead_advanced_ticks\": {},\n",
                        "  \"live_audio_meter_nonzero\": {},\n",
                        "  \"live_fade_reduced\": {},\n",
                        "  \"live_fade_recovered\": {},\n",
                        "  \"live_gain_reduced\": {},\n",
                        "  \"export_started\": {},\n",
                        "  \"export_progress_received\": {},\n",
                        "  \"playhead_advanced_while_exporting\": {},\n",
                        "  \"export_cancelled\": {}\n",
                        "}}\n"
                    ),
                    report.media_pool_drag_completed,
                    report.viewer_panel_height,
                    report.timeline_panel_height,
                    report.timeline_view_span_ticks,
                    report.timeline_end_ticks,
                    report.linked_video_bars,
                    report.linked_audio_bars,
                    report.analysis_metadata_ready,
                    report.waveform_ready,
                    report.waveform_peak_count,
                    report.monitor_frame_arrived,
                    report.native_viewer_uploaded,
                    report.playhead_advanced_ticks,
                    report.live_audio_meter_nonzero,
                    report.live_fade_reduced,
                    report.live_fade_recovered,
                    report.live_gain_reduced,
                    report.export_started,
                    report.export_progress_received,
                    report.playhead_advanced_while_exporting,
                    report.export_cancelled,
                );
                let _ = write_atomic_report(&path, &json);
            })
            .ok()?;
        Some(Self {
            media_id,
            media_pool_drag_completed: initial.media_pool_drag_completed,
            viewer_panel_height: initial.viewer_panel_height,
            timeline_panel_height: initial.timeline_panel_height,
            timeline_view_span_ticks: initial.timeline_view_span_ticks,
            timeline_end_ticks: initial.timeline_end_ticks,
            linked_video_bars: initial.linked_video_bars,
            linked_audio_bars: initial.linked_audio_bars,
            analysis_metadata_ready: false,
            waveform_ready: false,
            waveform_peak_count: 0,
            monitor_frame_arrived: false,
            native_viewer_uploaded: false,
            playback_start_tick: 0,
            playhead_advanced_ticks: 0,
            live_audio_meter_nonzero: false,
            pre_gain_meter_peak: 0.0,
            fade_reduction_requested_at_tick: None,
            live_fade_reduced: false,
            fade_clear_requested_at_tick: None,
            live_fade_recovered: false,
            gain_reduction_requested_at_tick: None,
            live_gain_reduced: false,
            export_started: false,
            export_progress_received: false,
            playhead_advanced_while_exporting: false,
            export_cancel_requested: false,
            export_cancelled: false,
            report_tx: Some(report_tx),
        })
    }

    fn record_analysis(&mut self, metadata_ready: bool, waveform_peak_count: usize) {
        self.analysis_metadata_ready |= metadata_ready;
        self.waveform_peak_count = self.waveform_peak_count.max(waveform_peak_count);
        self.waveform_ready |= waveform_peak_count > 0;
        self.publish_if_ready();
    }

    fn record_resolved_timeline(&mut self, view_span_ticks: i64, timeline_end_ticks: i64) {
        self.timeline_view_span_ticks = view_span_ticks;
        self.timeline_end_ticks = timeline_end_ticks;
        self.publish_if_ready();
    }

    fn record_monitor_frame(&mut self, media_id: u32, native_uploaded: bool) {
        let matches = media_id == self.media_id;
        self.monitor_frame_arrived |= matches;
        self.native_viewer_uploaded |= matches && native_uploaded;
        self.publish_if_ready();
    }

    fn record_playback(&mut self, playhead: i64, left: f32, right: f32, export_active: bool) {
        self.playhead_advanced_ticks = self
            .playhead_advanced_ticks
            .max(playhead.saturating_sub(self.playback_start_tick));
        let meter_peak = if left.is_finite() && right.is_finite() {
            left.abs().max(right.abs())
        } else {
            0.0
        };
        self.live_audio_meter_nonzero |= meter_peak > 0.0001;
        if let Some(requested_at) = self.fade_clear_requested_at_tick {
            self.live_fade_recovered |= playhead.saturating_sub(requested_at) >= 100_000
                && meter_peak >= (self.pre_gain_meter_peak * 0.10).max(0.0001);
        } else if let Some(requested_at) = self.fade_reduction_requested_at_tick {
            self.live_fade_reduced |= playhead.saturating_sub(requested_at) >= 100_000
                && meter_peak <= (self.pre_gain_meter_peak * 0.05).max(0.001);
        }
        if let Some(requested_at) = self.gain_reduction_requested_at_tick {
            self.live_gain_reduced |= playhead.saturating_sub(requested_at) >= 100_000
                && meter_peak <= (self.pre_gain_meter_peak * 0.05).max(0.001);
        } else if self.fade_reduction_requested_at_tick.is_none() {
            self.pre_gain_meter_peak = self.pre_gain_meter_peak.max(meter_peak);
        }
        self.playhead_advanced_while_exporting |= export_active && self.playhead_advanced_ticks > 0;
        self.publish_if_ready();
    }

    fn should_request_fade_reduction(&self) -> bool {
        self.fade_reduction_requested_at_tick.is_none()
            && self.playhead_advanced_ticks >= 250_000
            && self.pre_gain_meter_peak > 0.0001
    }

    fn record_fade_reduction_requested(&mut self, playhead: i64) {
        self.fade_reduction_requested_at_tick = Some(playhead);
    }

    fn should_clear_fade(&self) -> bool {
        self.live_fade_reduced && self.fade_clear_requested_at_tick.is_none()
    }

    fn record_fade_clear_requested(&mut self, playhead: i64) {
        self.fade_clear_requested_at_tick = Some(playhead);
    }

    fn should_request_gain_reduction(&self) -> bool {
        self.gain_reduction_requested_at_tick.is_none()
            && self.live_fade_recovered
            && self.pre_gain_meter_peak > 0.0001
    }

    fn record_gain_reduction_requested(&mut self, playhead: i64) {
        self.gain_reduction_requested_at_tick = Some(playhead);
    }

    fn record_export_started(&mut self, started: bool) {
        self.export_started |= started;
        self.publish_if_ready();
    }

    fn record_export_progress(&mut self) {
        self.export_progress_received = true;
        self.publish_if_ready();
    }

    fn should_cancel_export(&self) -> bool {
        self.export_started
            && self.export_progress_received
            && self.playhead_advanced_while_exporting
            && self.playhead_advanced_ticks >= 500_000
            && !self.export_cancel_requested
            && !self.export_cancelled
    }

    fn record_export_cancel_requested(&mut self) {
        self.export_cancel_requested = true;
    }

    fn record_export_cancelled(&mut self) {
        self.export_cancelled = true;
        self.publish_if_ready();
    }

    fn ready(&self) -> bool {
        self.media_pool_drag_completed
            && editor_layout_is_balanced(self.viewer_panel_height, self.timeline_panel_height)
            && timeline_view_fits_content(self.timeline_view_span_ticks, self.timeline_end_ticks)
            && self.linked_video_bars > 0
            && self.linked_audio_bars > 0
            && self.analysis_metadata_ready
            && self.waveform_ready
            && self.monitor_frame_arrived
            && self.native_viewer_uploaded
            && self.playhead_advanced_ticks >= 500_000
            && self.live_audio_meter_nonzero
            && self.live_fade_reduced
            && self.live_fade_recovered
            && self.live_gain_reduced
            && self.export_started
            && self.export_progress_received
            && self.playhead_advanced_while_exporting
            && self.export_cancelled
    }

    fn publish_if_ready(&mut self) {
        if !self.ready() {
            return;
        }
        if let Some(tx) = self.report_tx.take() {
            let _ = tx.try_send(MediaAcceptanceReport {
                media_pool_drag_completed: self.media_pool_drag_completed,
                viewer_panel_height: self.viewer_panel_height,
                timeline_panel_height: self.timeline_panel_height,
                timeline_view_span_ticks: self.timeline_view_span_ticks,
                timeline_end_ticks: self.timeline_end_ticks,
                linked_video_bars: self.linked_video_bars,
                linked_audio_bars: self.linked_audio_bars,
                analysis_metadata_ready: self.analysis_metadata_ready,
                waveform_ready: self.waveform_ready,
                waveform_peak_count: self.waveform_peak_count,
                monitor_frame_arrived: self.monitor_frame_arrived,
                native_viewer_uploaded: self.native_viewer_uploaded,
                playhead_advanced_ticks: self.playhead_advanced_ticks,
                live_audio_meter_nonzero: self.live_audio_meter_nonzero,
                live_fade_reduced: self.live_fade_reduced,
                live_fade_recovered: self.live_fade_recovered,
                live_gain_reduced: self.live_gain_reduced,
                export_started: self.export_started,
                export_progress_received: self.export_progress_received,
                playhead_advanced_while_exporting: self.playhead_advanced_while_exporting,
                export_cancelled: self.export_cancelled,
            });
        }
    }
}

fn nearest_rank_p95(samples: &[f32; FRAME_TIME_SAMPLE_COUNT]) -> f32 {
    let mut ordered = *samples;
    ordered.sort_unstable_by(f32::total_cmp);
    let index = ordered
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    ordered.get(index).copied().unwrap_or_default()
}

struct App {
    window: Option<Arc<Window>>,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    renderer: Option<SplashRenderer>,
    hub_renderer: Option<HubRenderer>,
    timeline_rect_scratch: Vec<RectInstance>,
    timeline_texture_scratch: Vec<TexturedRect>,
    frame_metrics: FrameMetrics,
    viewer_upload_timings: ViewerStageTimingWindow,
    surface_present_timings: ViewerStageTimingWindow,
    monitor_runtime_metrics: MonitorRuntimeMetrics,
    startup_presentation_probe: Option<StartupPresentationProbe>,
    surface_submission_probe: Option<SurfaceSubmissionProbe>,
    phase1_ui_probe: Option<phase1_ui::Probe>,
    playback_soak_probe: Option<PlaybackSoakProbe>,
    machine_profile: MachineProfile,
    renderer_report: Option<RendererReport>,
    monitor_cache_cap_bytes: usize,
    observed_decoder_backends: Vec<String>,
    observed_encoder_backend: Option<String>,
    media_acceptance_probe: Option<MediaAcceptanceProbe>,
    media_acceptance_pending_drag: Option<u32>,
    media_acceptance_export_path: Option<PathBuf>,
    external_drop_batch_next: Option<nle_timeline::Tick>,
    media_drag_pointer: MediaDragPointer,
    editor_modifiers: ModifiersState,
    egui_state: Option<egui_winit::State>,
    egui_context: egui::Context,
    pending_hub_backdrops: Option<[ThumbnailRgba; 2]>,
    hub_backdrop_textures: Option<[egui::TextureHandle; 2]>,
    project_thumbnail_textures: HashMap<u32, egui::TextureHandle>,
    video_strip_textures: HashMap<u32, egui::TextureHandle>,
    video_strips: HashMap<u32, Arc<nle_waveform::VideoStrip>>,
    video_strip_order: VecDeque<u32>,
    video_strip_bytes: usize,
    monitor_decoders: [nle_decode::MonitorDecoder; MONITOR_LAYER_COUNT],
    monitor_frame_cache_pool: nle_decode::MonitorFrameCachePool,
    monitor_session_pool: nle_decode::MonitorSessionPool,
    monitor_source_coordinator: nle_decode::MonitorSourceCoordinator,
    audio_engine: Option<nle_audio::AudioEngine>,
    audio_engine_error: Option<String>,
    startup_resources_tx: Option<mpsc::Sender<StartupResources>>,
    startup_resources_rx: mpsc::Receiver<StartupResources>,
    startup_resources_notify: Arc<dyn Fn() + Send + Sync>,
    startup_resources_started: bool,
    startup_resources_ready: bool,
    preloaded_models: model_preload::PreloadedModels,
    audio_engine_initialized: bool,
    audio_transport: Option<AudioTransportState>,
    monitor_textures: [Option<egui::TextureHandle>; MONITOR_LAYER_COUNT],
    monitor_last_proxy_frames: [Option<ScrubProxyKey>; MONITOR_LAYER_COUNT],
    monitor_last_requests: [Option<MonitorRequestKey>; MONITOR_LAYER_COUNT],
    monitor_source_identities: [Option<MonitorSourceIdentity>; MONITOR_LAYER_COUNT],
    monitor_generations: [u64; MONITOR_LAYER_COUNT],
    monitor_latest_request_ids: [u64; MONITOR_LAYER_COUNT],
    monitor_next_request_id: u64,
    monitor_requests_in_flight: [bool; MONITOR_LAYER_COUNT],
    monitor_request_deferred: [bool; MONITOR_LAYER_COUNT],
    monitor_admission_priorities: [u8; MONITOR_LAYER_COUNT],
    monitor_request_started_at: [Option<(u64, Instant)>; MONITOR_LAYER_COUNT],
    adaptive_preview: AdaptivePreviewController,
    hub: ProjectHubState,
    catalog_path: Option<PathBuf>,
    project_paths: HashMap<u32, PathBuf>,
    current_project_id: Option<u32>,
    current_project_settings: ProjectSettings,
    project_writer: ProjectWriter,
    catalog_writer: CatalogWriter,
    /// Last persisted durable revision. Unlike a snapshot comparison this is O(1) per frame.
    last_enqueued_generation: Option<u64>,
    autosave_schedule: AutosaveSchedule,
    pending_thumbnail: Option<ThumbnailRgba>,
    project_save_blocked: bool,
    export_job: Option<nle_export::ExportJob>,
    export_project_id: Option<u32>,
    upscale_job: Option<nle_upscale::UpscaleJob>,
    proxy_job: Option<nle_proxy::ProxyJob>,
    proxy_job_media_id: Option<u32>,
    proxy_delete_job: Option<nle_proxy::ProxyDeleteJob>,
    proxy_delete_media_id: Option<u32>,
    proxy_records: HashMap<u32, ProxyRecord>,
    proxy_cache_root: PathBuf,
    editor: EditorState,
    project_dialog_tx: mpsc::Sender<ProjectDialogResult>,
    project_dialog_rx: mpsc::Receiver<ProjectDialogResult>,
    project_dialog_notify: Arc<dyn Fn() + Send + Sync>,
    media_dialog_tx: mpsc::Sender<Vec<PathBuf>>,
    media_dialog_rx: mpsc::Receiver<Vec<PathBuf>>,
    media_analysis_tx: mpsc::Sender<MediaAnalysisResult>,
    media_analysis_rx: mpsc::Receiver<MediaAnalysisResult>,
    media_analysis_pending: VecDeque<(u64, u32, PathBuf)>,
    media_analysis_in_flight: HashSet<(u64, u32)>,
    media_analysis_cancellations: HashMap<(u64, u32), Arc<AtomicBool>>,
    media_analysis_workers: HashMap<(u64, u32), thread::JoinHandle<()>>,
    media_analysis_epoch: u64,
    monitor_cache_epoch: u64,
    hardware_tx: Option<mpsc::Sender<HardwareProfile>>,
    hardware_rx: mpsc::Receiver<HardwareProfile>,
    hardware_detection_started_at: Option<Instant>,
    hardware_profile: Option<HardwareProfile>,
    screen: Screen,
    splash_first_presented_at: Option<Instant>,
    first_surface_presented: bool,
    app_resources_ready: bool,
    splash_continue_available: bool,
    started_at: Instant,
}

#[derive(Clone, Debug)]
struct ProxyRecord {
    artifact: nle_proxy::ProxyArtifact,
    enabled: bool,
}

fn resolved_monitor_media_path<'a>(
    records: &'a HashMap<u32, ProxyRecord>,
    media_id: u32,
    original: &'a Path,
) -> &'a Path {
    records
        .get(&media_id)
        .filter(|record| record.enabled)
        .map(|record| record.artifact.path.as_path())
        .unwrap_or(original)
}

fn proxy_text(language: Language, english: &str, japanese: &str) -> String {
    match language {
        Language::English => english,
        Language::Japanese => japanese,
    }
    .to_owned()
}

fn proxy_error_text(
    language: Language,
    english_prefix: &str,
    japanese_prefix: &str,
    detail: &str,
) -> String {
    match language {
        Language::English => format!("{english_prefix}: {detail}"),
        Language::Japanese => format!("{japanese_prefix}: {detail}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Splash,
    ProjectHub,
    Editor,
}

#[derive(Clone, Copy, Debug)]
enum AppEvent {
    Monitor,
    ProjectWriter,
    ProjectDialog,
    StartupResources,
}

struct StartupResources {
    catalog: Option<(Vec<nle_ui_core::Project>, HashMap<u32, PathBuf>)>,
    thumbnails: Vec<(u32, ThumbnailRgba)>,
    thumbnail_error: Option<String>,
    preloaded_models: model_preload::PreloadedModels,
    model_errors: Vec<String>,
}

fn load_startup_resources(catalog_path: Option<PathBuf>) -> StartupResources {
    let model_directory = model_preload::packaged_model_directory();
    load_startup_resources_from(catalog_path, model_directory.as_deref())
}

fn load_startup_resources_from(
    catalog_path: Option<PathBuf>,
    model_directory: Option<&Path>,
) -> StartupResources {
    let model_preload = model_preload::preload_models(model_directory);
    let mut catalog = catalog_path.as_deref().map(load_catalog_with_paths);
    if let (Some(path), Some((projects, paths))) = (catalog_path.as_deref(), catalog.as_mut()) {
        refresh_project_file_sizes(path, projects, paths);
    }
    let mut thumbnails = Vec::new();
    let mut thumbnail_error = None;
    if let (Some(path), Some((projects, _))) = (catalog_path.as_deref(), catalog.as_ref()) {
        for project in projects {
            let thumbnail_path = project_thumbnail_path(path, project.id);
            let Ok(bytes) = fs::read(&thumbnail_path) else {
                continue;
            };
            match image::load_from_memory(&bytes) {
                Ok(image) => {
                    let image = image.into_rgba8();
                    thumbnails.push((
                        project.id,
                        ThumbnailRgba {
                            width: image.width(),
                            height: image.height(),
                            rgba: image.into_raw(),
                        },
                    ));
                }
                Err(error) => {
                    thumbnail_error = Some(format!(
                        "Could not load project thumbnail {}: {error}",
                        thumbnail_path.display()
                    ));
                }
            }
        }
    }
    StartupResources {
        catalog,
        thumbnails,
        thumbnail_error,
        preloaded_models: model_preload.models,
        model_errors: model_preload.errors,
    }
}

fn refresh_project_file_sizes(
    catalog_path: &std::path::Path,
    projects: &mut [nle_ui_core::Project],
    paths: &HashMap<u32, PathBuf>,
) {
    for project in projects {
        let primary = paths
            .get(&project.id)
            .cloned()
            .unwrap_or_else(|| project_document_path(catalog_path, project.id));
        let metadata = fs::metadata(&primary).ok().or_else(|| {
            (primary == project_document_path(catalog_path, project.id))
                .then(|| fs::metadata(legacy_project_document_path(catalog_path, project.id)).ok())
                .flatten()
        });
        if let Some(metadata) = metadata {
            project.size = format_file_size(metadata.len());
        }
    }
}

impl App {
    fn new(notify: impl Fn(AppEvent) + Send + Sync + 'static) -> Self {
        let demo_hub = std::env::var("MAELSTROM_DEMO_HUB").as_deref() == Ok("1");
        let notify = Arc::new(notify);
        let monitor_notify = Arc::clone(&notify);
        let writer_notify = Arc::clone(&notify);
        let catalog_notify = Arc::clone(&notify);
        let dialog_notify = Arc::clone(&notify);
        let startup_notify = Arc::clone(&notify);
        Self::new_with_catalog_and_notifier(
            demo_hub,
            (!demo_hub && std::env::var_os("MAELSTROM_PHASE1_UI_CONFIG").is_none())
                .then(project_catalog_path),
            move || monitor_notify(AppEvent::Monitor),
            move || writer_notify(AppEvent::ProjectWriter),
            move || catalog_notify(AppEvent::ProjectWriter),
            move || dialog_notify(AppEvent::ProjectDialog),
            move || startup_notify(AppEvent::StartupResources),
            None,
        )
    }

    #[cfg(test)]
    fn new_with_catalog(demo_hub: bool, catalog_path: Option<PathBuf>) -> Self {
        let startup_path = catalog_path.clone();
        let mut app = Self::new_with_catalog_and_notifier(
            demo_hub,
            catalog_path,
            || {},
            || {},
            || {},
            || {},
            || {},
            None,
        );
        app.startup_resources_tx = None;
        app.apply_startup_resources(load_startup_resources(startup_path));
        app.initialize_audio_engine_after_first_frame();
        app
    }

    /// Test-only app construction for monitor event contracts that must not load startup/model
    /// resources or initialize a native audio device.
    #[cfg(test)]
    fn new_without_startup_or_audio_for_monitor_contract() -> Self {
        let mut app = Self::new_with_catalog_and_notifier(
            false,
            None,
            || {},
            || {},
            || {},
            || {},
            || {},
            None,
        );
        // No startup worker will send on this receiver in this isolated app path.
        app.startup_resources_tx = None;
        app
    }

    #[cfg(test)]
    fn new_with_catalog_and_monitor_cache_bytes(
        demo_hub: bool,
        catalog_path: Option<PathBuf>,
        monitor_cache_bytes: usize,
    ) -> Self {
        let startup_path = catalog_path.clone();
        let mut app = Self::new_with_catalog_and_notifier(
            demo_hub,
            catalog_path,
            || {},
            || {},
            || {},
            || {},
            || {},
            Some(monitor_cache_bytes),
        );
        app.startup_resources_tx = None;
        app.apply_startup_resources(load_startup_resources(startup_path));
        app.initialize_audio_engine_after_first_frame();
        app
    }

    // Startup notifiers stay separate so each background owner can wake the event loop directly.
    #[allow(clippy::too_many_arguments)]
    fn new_with_catalog_and_notifier(
        demo_hub: bool,
        catalog_path: Option<PathBuf>,
        monitor_notifier: impl Fn() + Send + Sync + 'static,
        writer_notifier: impl Fn() + Send + Sync + 'static,
        catalog_writer_notifier: impl Fn() + Send + Sync + 'static,
        project_dialog_notifier: impl Fn() + Send + Sync + 'static,
        startup_resources_notifier: impl Fn() + Send + Sync + 'static,
        monitor_cache_bytes_override: Option<usize>,
    ) -> Self {
        let started_at = Instant::now();
        let (project_dialog_tx, project_dialog_rx) = mpsc::channel();
        let (media_dialog_tx, media_dialog_rx) = mpsc::channel();
        let (media_analysis_tx, media_analysis_rx) = mpsc::channel();
        let (hardware_tx, hardware_rx) = mpsc::channel();
        let (startup_resources_tx, startup_resources_rx) = mpsc::channel();
        let hub = ProjectHubState::new(demo_hub);
        let editor = EditorState::new(Language::English, "Untitled Project");
        let monitor_notifier: Arc<dyn Fn() + Send + Sync> = Arc::new(monitor_notifier);
        let monitor_cache_bytes = monitor_cache_bytes_override
            .unwrap_or_else(|| monitor_cache_bytes_from_args(std::env::args()));
        let monitor_frame_cache_pool = nle_decode::MonitorFrameCachePool::new(monitor_cache_bytes);
        let monitor_session_pool = nle_decode::MonitorSessionPool::new(
            MONITOR_FOREGROUND_SESSION_CAP,
            MONITOR_BACKGROUND_SESSION_CAP,
        );
        let monitor_source_coordinator = nle_decode::MonitorSourceCoordinator::new(
            MONITOR_LAYER_COUNT,
            monitor_session_pool.clone(),
        );
        let monitor_decoders = std::array::from_fn(|_| {
            let notifier = Arc::clone(&monitor_notifier);
            nle_decode::MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
                move || notifier(),
                monitor_frame_cache_pool.clone(),
                monitor_source_coordinator.clone(),
            )
        });
        Self {
            window: None,
            surface: None,
            surface_config: None,
            device: None,
            queue: None,
            renderer: None,
            hub_renderer: None,
            timeline_rect_scratch: Vec::with_capacity(64 * 1024),
            timeline_texture_scratch: Vec::with_capacity(16 * 1024),
            frame_metrics: FrameMetrics::default(),
            viewer_upload_timings: ViewerStageTimingWindow::default(),
            surface_present_timings: ViewerStageTimingWindow::default(),
            monitor_runtime_metrics: MonitorRuntimeMetrics::default(),
            startup_presentation_probe: StartupPresentationProbe::from_environment(started_at),
            surface_submission_probe: SurfaceSubmissionProbe::from_environment(),
            phase1_ui_probe: phase1_ui::Probe::from_environment(),
            playback_soak_probe: PlaybackSoakProbe::from_environment(),
            machine_profile: hardware::detect_machine(),
            renderer_report: None,
            monitor_cache_cap_bytes: monitor_cache_bytes,
            // DecodeBackend currently has six variants; preallocate all of them so observing a
            // fallback never reallocates in the decoder event hot path.
            observed_decoder_backends: Vec::with_capacity(6),
            observed_encoder_backend: None,
            media_acceptance_probe: None,
            media_acceptance_pending_drag: None,
            media_acceptance_export_path: None,
            external_drop_batch_next: None,
            media_drag_pointer: MediaDragPointer::default(),
            editor_modifiers: ModifiersState::default(),
            egui_state: None,
            egui_context: egui::Context::default(),
            pending_hub_backdrops: None,
            hub_backdrop_textures: None,
            project_thumbnail_textures: HashMap::new(),
            video_strip_textures: HashMap::new(),
            video_strips: HashMap::new(),
            video_strip_order: VecDeque::new(),
            video_strip_bytes: 0,
            monitor_decoders,
            monitor_frame_cache_pool,
            monitor_session_pool,
            monitor_source_coordinator,
            audio_engine: None,
            audio_engine_error: None,
            startup_resources_tx: Some(startup_resources_tx),
            startup_resources_rx,
            startup_resources_notify: Arc::new(startup_resources_notifier),
            startup_resources_started: false,
            startup_resources_ready: false,
            preloaded_models: model_preload::PreloadedModels::default(),
            audio_engine_initialized: false,
            audio_transport: None,
            monitor_textures: std::array::from_fn(|_| None),
            monitor_last_proxy_frames: [None; MONITOR_LAYER_COUNT],
            monitor_last_requests: [None; MONITOR_LAYER_COUNT],
            monitor_source_identities: std::array::from_fn(|_| None),
            monitor_generations: [1; MONITOR_LAYER_COUNT],
            monitor_latest_request_ids: [0; MONITOR_LAYER_COUNT],
            monitor_next_request_id: 1,
            monitor_requests_in_flight: [false; MONITOR_LAYER_COUNT],
            monitor_request_deferred: [false; MONITOR_LAYER_COUNT],
            monitor_admission_priorities: [0; MONITOR_LAYER_COUNT],
            monitor_request_started_at: [None; MONITOR_LAYER_COUNT],
            adaptive_preview: AdaptivePreviewController::default(),
            hub,
            catalog_path,
            project_paths: HashMap::new(),
            current_project_id: None,
            current_project_settings: ProjectSettings::default(),
            project_writer: ProjectWriter::new_with_notifier(writer_notifier),
            catalog_writer: CatalogWriter::new_with_notifier(catalog_writer_notifier),
            last_enqueued_generation: None,
            autosave_schedule: AutosaveSchedule::default(),
            pending_thumbnail: None,
            project_save_blocked: false,
            export_job: None,
            export_project_id: None,
            upscale_job: None,
            proxy_job: None,
            proxy_job_media_id: None,
            proxy_delete_job: None,
            proxy_delete_media_id: None,
            proxy_records: HashMap::new(),
            proxy_cache_root: proxy_cache_root(),
            editor,
            project_dialog_tx,
            project_dialog_rx,
            project_dialog_notify: Arc::new(project_dialog_notifier),
            media_dialog_tx,
            media_dialog_rx,
            media_analysis_tx,
            media_analysis_rx,
            media_analysis_pending: VecDeque::new(),
            media_analysis_in_flight: HashSet::new(),
            media_analysis_cancellations: HashMap::new(),
            media_analysis_workers: HashMap::new(),
            media_analysis_epoch: 0,
            monitor_cache_epoch: 0,
            hardware_tx: Some(hardware_tx),
            hardware_rx,
            hardware_detection_started_at: None,
            hardware_profile: None,
            screen: Screen::Splash,
            splash_first_presented_at: None,
            first_surface_presented: false,
            app_resources_ready: false,
            splash_continue_available: false,
            started_at,
        }
    }

    fn create_gpu(&mut self, window: Arc<Window>) {
        let phase0_adapter_class = phase0_surface_adapter_class_from_environment()
            .unwrap_or_else(|error| panic!("invalid Phase 0 surface adapter selection: {error}"));
        let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        if phase0_adapter_class.is_some() {
            instance_descriptor.backends = wgpu::Backends::DX12;
        }
        let instance = wgpu::Instance::new(instance_descriptor);
        let surface = instance
            .create_surface(window.clone())
            .expect("create splash surface");
        let adapter = match phase0_adapter_class {
            Some(class) => select_phase0_surface_adapter(&instance, &surface, class)
                .unwrap_or_else(|error| panic!("select Phase 0 surface adapter: {error}")),
            None => pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
            .expect("find graphics adapter"),
        };
        let adapter_info = adapter.get_info();
        tracing::info!(
            target: "maelstrom::gpu",
            name = %adapter_info.name,
            vendor = adapter_info.vendor,
            device = adapter_info.device,
            device_type = ?adapter_info.device_type,
            backend = ?adapter_info.backend,
            "selected renderer adapter"
        );
        self.renderer_report = Some(RendererReport {
            name: adapter_info.name.clone(),
            vendor_id: adapter_info.vendor,
            device_id: adapter_info.device,
            device_type: format!("{:?}", adapter_info.device_type),
            backend: format!("{:?}", adapter_info.backend),
            driver: adapter_info.driver.clone(),
            driver_info: adapter_info.driver_info.clone(),
        });
        let timestamp_query = wgpu::Features::TIMESTAMP_QUERY;
        let optional_renderer_features = if adapter.features().contains(timestamp_query) {
            timestamp_query
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("Maelstrom splash device"),
            required_features: optional_renderer_features,
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("create graphics device");
        let size = window.inner_size();
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(capabilities.formats[0]);
        let present_mode = capabilities
            .present_modes
            .iter()
            .copied()
            .find(|mode| *mode == wgpu::PresentMode::Fifo)
            .unwrap_or(capabilities.present_modes[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        let hub_backdrops = [
            decode_embedded_rgba(ENGLISH_SPLASH),
            decode_embedded_rgba(JAPANESE_SPLASH),
        ];
        let renderer = SplashRenderer::new_rgba(
            &device,
            &queue,
            format,
            config.width,
            config.height,
            SplashRgba {
                width: hub_backdrops[0].width,
                height: hub_backdrops[0].height,
                pixels: &hub_backdrops[0].rgba,
            },
            SplashRgba {
                width: hub_backdrops[1].width,
                height: hub_backdrops[1].height,
                pixels: &hub_backdrops[1].rgba,
            },
        );
        self.window = Some(window);
        self.surface = Some(surface);
        self.surface_config = Some(config);
        self.device = Some(device);
        self.queue = Some(queue);
        self.renderer = Some(renderer);
        self.pending_hub_backdrops = Some(hub_backdrops);
    }

    fn try_publish_surface_submission_report(&mut self) {
        if self
            .surface_submission_probe
            .as_ref()
            .is_none_or(|probe| probe.completed.is_none())
        {
            return;
        }
        let full_media_smoke = std::env::var_os("MAELSTROM_MEDIA_ACCEPTANCE_REPORT").is_some();
        if !surface_report_backends_ready(
            full_media_smoke,
            &self.observed_decoder_backends,
            self.observed_encoder_backend.as_deref(),
        ) {
            return;
        }
        let decoder_stage_timings = aggregate_monitor_decoder_stage_timings(&self.monitor_decoders);
        if !surface_report_stage_timings_ready(full_media_smoke, &decoder_stage_timings) {
            return;
        }
        let viewer_stage_timings = ViewerStageTimingsReport {
            upload_cpu: self.viewer_upload_timings.snapshot(),
            compositor_encode_cpu: self
                .hub_renderer
                .as_ref()
                .map(HubRenderer::viewer_compositor_encode_timing)
                .unwrap_or_default()
                .into(),
        };
        if !surface_report_viewer_stage_timings_ready(full_media_smoke, viewer_stage_timings) {
            return;
        }
        let gpu_stage_timings = self
            .hub_renderer
            .as_ref()
            .map(|renderer| {
                GpuStageTimingsReport::from_snapshots(
                    renderer.viewer_compositor_gpu_timing(),
                    renderer.gpu_submission_completion_timing(),
                )
            })
            .unwrap_or_default();
        if !surface_report_gpu_stage_timings_ready(full_media_smoke, gpu_stage_timings) {
            return;
        }
        let audio_diagnostics = self
            .audio_engine
            .as_ref()
            .map(nle_audio::AudioEngine::runtime_diagnostics)
            .unwrap_or_default();
        let audio_stage_timings = AudioStageTimingsReport {
            output_callback_cpu: audio_diagnostics.output_callback_cpu_timing.into(),
            mix_render_cpu: audio_diagnostics.mix_render_cpu_timing.into(),
        };
        if !surface_report_audio_stage_timings_ready(full_media_smoke, audio_stage_timings) {
            return;
        }
        let Some(renderer) = self.renderer_report.clone() else {
            return;
        };
        let preview = preview_request(&self.editor);
        let environment = SurfaceReportEnvironment {
            renderer,
            decoder_backends: self.observed_decoder_backends.clone(),
            encoder_backend: self
                .observed_encoder_backend
                .clone()
                .unwrap_or_else(|| "not_observed".to_owned()),
            machine: self.machine_profile.clone(),
            selected_preview_quality: format!("{:?}", self.editor.preview_quality()),
            resolved_preview_quality: format!("{:?}", self.editor.resolved_preview_quality()),
            preview_size: preview.output_size,
            monitor_cache_cap_bytes: self.monitor_cache_cap_bytes,
            display_refresh_millihertz: self
                .window
                .as_ref()
                .and_then(|window| window.current_monitor())
                .and_then(|monitor| monitor.refresh_rate_millihertz()),
            decoder_stage_timings,
            viewer_stage_timings,
            gpu_stage_timings,
            audio_stage_timings,
            runtime_diagnostics: RuntimeDiagnosticsReport::from(
                self.runtime_diagnostics_with_audio(audio_diagnostics),
            ),
        };
        let published = self
            .surface_submission_probe
            .as_mut()
            .is_some_and(|probe| probe.publish(environment));
        if published {
            self.surface_submission_probe = None;
        }
    }

    fn runtime_diagnostics(&self) -> RuntimeDiagnostics {
        let audio = self
            .audio_engine
            .as_ref()
            .map(nle_audio::AudioEngine::runtime_diagnostics)
            .unwrap_or_default();
        self.runtime_diagnostics_with_audio(audio)
    }

    fn runtime_diagnostics_with_audio(
        &self,
        audio: nle_audio::AudioRuntimeDiagnostics,
    ) -> RuntimeDiagnostics {
        let mut diagnostics = self.monitor_runtime_metrics.diagnostics(
            audio.underrun_device_frames,
            audio.callback_lock_failures,
            audio.late_decoded_frames_discarded,
        );
        diagnostics.live_pipeline_timing = self.live_pipeline_timing(audio);
        diagnostics
    }

    fn live_pipeline_timing(
        &self,
        audio: nle_audio::AudioRuntimeDiagnostics,
    ) -> LivePipelineTiming {
        let decoder = aggregate_monitor_decoder_stage_timings(&self.monitor_decoders);
        let mut timing = LivePipelineTiming {
            active_video_layers: self.editor.playback_targets().count(),
            selected_preview_quality: self.editor.preview_quality(),
            resolved_preview_quality: self.editor.resolved_preview_quality(),
            ..Default::default()
        };
        for (stage, sample) in [
            (
                LivePipelineTimingStage::Demux,
                live_mean_stage_sample(decoder.demux_packet),
            ),
            (
                LivePipelineTimingStage::Decode,
                live_mean_stage_sample(decoder.decoder_calls),
            ),
            (
                LivePipelineTimingStage::HardwareTransfer,
                live_mean_stage_sample(decoder.hardware_transfer),
            ),
            (
                LivePipelineTimingStage::Scale,
                live_mean_stage_sample(decoder.scaler),
            ),
            (
                LivePipelineTimingStage::RgbaPacking,
                live_mean_stage_sample(decoder.rgba_copy_letterbox),
            ),
        ] {
            timing.set_sample(stage, sample);
        }

        let upload = self.viewer_upload_timings.snapshot();
        timing.set_sample(
            LivePipelineTimingStage::ViewerUpload,
            live_p95_stage_sample(upload.samples, upload.p95_ms, upload.max_ms),
        );
        if let Some(renderer) = &self.hub_renderer {
            let compositor_cpu = renderer.try_viewer_compositor_encode_timing();
            timing.set_sample(
                LivePipelineTimingStage::CompositorCpuEncode,
                compositor_cpu.and_then(|sample| {
                    live_p95_stage_sample(sample.samples, sample.p95_ms, sample.max_ms)
                }),
            );
            let compositor_gpu = renderer.viewer_compositor_gpu_timing();
            timing.set_sample(
                LivePipelineTimingStage::CompositorGpu,
                compositor_gpu
                    .supported
                    .then(|| {
                        live_p95_stage_sample(
                            compositor_gpu.samples,
                            compositor_gpu.p95_ms,
                            compositor_gpu.max_ms,
                        )
                    })
                    .flatten(),
            );
            let submission = renderer.gpu_submission_completion_timing();
            timing.set_sample(
                LivePipelineTimingStage::GpuSubmitToCompletion,
                live_p95_stage_sample(submission.samples, submission.p95_ms, submission.max_ms),
            );
        }
        timing.set_sample(
            LivePipelineTimingStage::AudioMix,
            live_audio_mean_stage_sample(audio.mix_render_cpu_timing),
        );
        let present = self.surface_present_timings.snapshot();
        timing.set_sample(
            LivePipelineTimingStage::SurfacePresentCall,
            live_p95_stage_sample(present.samples, present.p95_ms, present.max_ms),
        );
        timing
    }

    /// Advances the opt-in, wall-clock soak only from the UI-owned transport path. The native
    /// audio callback remains untouched. Rewinding occurs only after the logical A/V timeline
    /// reaches its end, so every loop takes the normal seek/decode/audio reconciliation path.
    fn advance_playback_soak(&mut self, now: Instant) {
        if self.playback_soak_probe.is_none() {
            return;
        }
        let Some(mut probe) = self.playback_soak_probe.take() else {
            return;
        };
        let diagnostics = self.runtime_diagnostics();
        let timeline_end = self.editor.timeline_end().0;
        let reached_timeline_end = timeline_end > 0 && self.editor.playhead.0 >= timeline_end;
        let audio_transport_active = self.audio_transport.is_some();
        let audio_error_present = self.audio_engine_error.is_some();
        if audio_transport_active && self.editor.playing && !audio_error_present {
            probe.start_after_real_playback(now, diagnostics);
        }
        probe.observe_transport_state(
            audio_error_present,
            self.editor.playing,
            audio_transport_active,
            reached_timeline_end,
        );
        let audio_transport_healthy_now = !audio_error_present
            && ((self.editor.playing && audio_transport_active)
                || (!self.editor.playing && reached_timeline_end));
        if probe.due(now) {
            if let Some(report) = probe.report(
                now,
                diagnostics,
                self.observed_decoder_backends.clone(),
                format!("{:?}", self.editor.preview_quality()),
                format!("{:?}", self.editor.resolved_preview_quality()),
                self.monitor_cache_cap_bytes,
                aggregate_playback_soak_monitor_resources(
                    &self.monitor_frame_cache_pool,
                    self.monitor_session_pool.diagnostics(),
                    self.monitor_source_coordinator.diagnostics(),
                ),
                aggregate_monitor_decoder_stage_timings(&self.monitor_decoders),
                audio_transport_healthy_now,
            ) {
                let _ = probe.publish(report);
            }
            if let Some(window) = &self.window {
                window.set_window_level(WindowLevel::Normal);
            }
            return;
        }
        if probe.is_started() && !self.editor.playing && reached_timeline_end {
            self.editor.set_playhead(nle_timeline::Tick(0));
            self.editor.start_playback();
            self.audio_transport = None;
            probe.record_loop();
        }
        self.playback_soak_probe = Some(probe);
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let (Some(surface), Some(config), Some(device), Some(renderer)) = (
            &self.surface,
            &mut self.surface_config,
            &self.device,
            &mut self.renderer,
        ) else {
            return;
        };
        config.width = width;
        config.height = height;
        surface.configure(device, config);
        renderer.resize(device, width, height);
    }

    #[cfg(debug_assertions)]
    fn set_vsync(&mut self, enabled: bool) {
        let (Some(surface), Some(config), Some(device)) =
            (&self.surface, &mut self.surface_config, &self.device)
        else {
            return;
        };
        config.present_mode = if enabled {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        surface.configure(device, config);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// HOT PATH — no IO. Polls no worker and calls no media codec.
    fn render(&mut self) {
        let Some(config) = self.surface_config.clone() else {
            return;
        };
        let Some(device) = self.device.clone() else {
            return;
        };
        let Some(queue) = self.queue.clone() else {
            return;
        };
        let frame = {
            let Some(surface) = self.surface.as_ref() else {
                return;
            };
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame) => frame,
                // Render this frame; the next resize/outdated acquisition will reconfigure.
                wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                wgpu::CurrentSurfaceTexture::Outdated
                | wgpu::CurrentSurfaceTexture::Lost
                | wgpu::CurrentSurfaceTexture::Timeout
                | wgpu::CurrentSurfaceTexture::Occluded
                | wgpu::CurrentSurfaceTexture::Validation => {
                    surface.configure(&device, &config);
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return;
                }
            }
        };
        let frame_cpu_started = Instant::now();
        let mut native_primitive_counts = (0, 0);
        let view = frame.texture.create_view(&Default::default());
        let mut hub_action = None;
        let mut editor_action = None;
        if self.screen == Screen::Splash {
            if let Some(splash) = self.renderer.as_ref() {
                splash.render(
                    &device,
                    &queue,
                    &view,
                    self.started_at.elapsed().as_secs_f32(),
                );
            }
            self.splash_first_presented_at
                .get_or_insert_with(Instant::now);
            self.paint_splash_loading_overlay(&view);
        } else {
            let window = self.window.as_ref().expect("window lives while rendering");
            let state = self.egui_state.as_mut().expect("application event state");
            let mut raw_input = state.take_egui_input(window);
            let measured_input = self
                .phase1_ui_probe
                .as_mut()
                .is_some_and(|probe| probe.inject_input(&mut raw_input, &self.editor));
            // HOT PATH — no IO. Dialogs and media work are dispatched after drawing.
            let hub_backdrops = self
                .hub_backdrop_textures
                .as_ref()
                .map(|textures| HubBackdrops {
                    english: textures[0].id(),
                    japanese: textures[1].id(),
                    image_size: textures[0].size_vec2(),
                });
            // Register callback resources before the UI emits its timeline PaintCallback.
            let hub_renderer = self
                .hub_renderer
                .get_or_insert_with(|| HubRenderer::new(&device, config.format));
            let timeline_rect_callback = hub_renderer.timeline_rects();
            let timeline_texture_callback = hub_renderer.timeline_textures();
            let viewer_compositor_callback = hub_renderer.viewer_compositor();
            let screen = self.screen;
            let context = self.egui_context.clone();
            let hub = &mut self.hub;
            let editor = &mut self.editor;
            self.timeline_rect_scratch.clear();
            self.timeline_texture_scratch.clear();
            let mut timeline_canvas = NativeTimelineCanvas {
                rect_callback: timeline_rect_callback,
                texture_callback: timeline_texture_callback,
                rect_scratch: &mut self.timeline_rect_scratch,
                texture_scratch: &mut self.timeline_texture_scratch,
            };
            let mut viewer_canvas = NativeViewerCanvas::new(viewer_compositor_callback);
            let output = context.run_ui(raw_input, |ui| match screen {
                Screen::ProjectHub => {
                    show_with_backdrops(ui, hub, hub_backdrops);
                    hub_action = hub.take_action();
                }
                Screen::Editor => {
                    show_editor_with_canvases(ui, editor, &mut timeline_canvas, &mut viewer_canvas);
                    editor_action = editor.take_action();
                }
                Screen::Splash => {}
            });
            if let Some(probe) = &mut self.phase1_ui_probe {
                probe.after_ui(editor, measured_input);
            }
            native_primitive_counts = (
                timeline_canvas.rect_scratch.len(),
                timeline_canvas.texture_scratch.len(),
            );
            timeline_canvas.submit();
            viewer_canvas.submit();
            state.handle_platform_output(window, output.platform_output);
            let primitives = context.tessellate(output.shapes, output.pixels_per_point);
            let measure_gpu_completion = self.screen == Screen::Editor
                && (self.surface_submission_probe.is_some() || self.phase1_ui_probe.is_some());
            let hub_renderer = self
                .hub_renderer
                .get_or_insert_with(|| HubRenderer::new(&device, config.format));
            let screen_descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [config.width, config.height],
                pixels_per_point: output.pixels_per_point,
            };
            if measure_gpu_completion {
                hub_renderer.render_with_gpu_completion_measurement(
                    &device,
                    &queue,
                    &view,
                    &primitives,
                    &output.textures_delta,
                    screen_descriptor,
                );
            } else {
                hub_renderer.render(
                    &device,
                    &queue,
                    &view,
                    &primitives,
                    &output.textures_delta,
                    screen_descriptor,
                );
            }
            if output
                .viewport_output
                .values()
                .any(|v| v.repaint_delay.is_zero())
            {
                window.request_redraw();
            }
        }
        // A ruler drag mutates the playhead while building this egui frame. Publish its decode
        // target before presenting so the worker can begin immediately instead of waiting for
        // the next surface/event turn. The normal post-present sync remains authoritative for
        // editor actions and is allocation-free when this request already matches.
        if self.screen == Screen::Editor && self.editor.is_scrubbing() {
            self.sync_monitor_decode();
            self.capture_phase1_ui_targets();
        }
        let frame_cpu_duration = frame_cpu_started.elapsed();
        let frame_performance = self.frame_metrics.record(
            frame_cpu_duration,
            native_primitive_counts.0,
            native_primitive_counts.1,
        );
        let present_call_started = Instant::now();
        frame.present();
        let present_call_duration = present_call_started.elapsed();
        self.surface_present_timings.record(present_call_duration);
        let presented_at = Instant::now();
        if let Some(probe) = &mut self.startup_presentation_probe {
            probe.record_first_present(presented_at);
        }
        self.first_surface_presented = true;
        if let Some(probe) = &mut self.surface_submission_probe {
            let collecting = probe.record(frame_cpu_duration, present_call_duration, presented_at);
            if let Some(window) = &self.window {
                if collecting {
                    window.request_redraw();
                } else if self.playback_soak_probe.is_none() {
                    window.set_window_level(WindowLevel::Normal);
                }
            }
        }
        self.try_publish_surface_submission_report();
        if let Some(action) = hub_action {
            self.handle_hub_action(action);
        }
        if let Some(action) = editor_action {
            self.handle_editor_action(action);
        }
        self.advance_media_acceptance_drag_smoke();
        if self.screen == Screen::Editor {
            let performance_hud_rebuilt = if let Some(performance) = frame_performance {
                self.editor
                    .set_runtime_diagnostics(self.runtime_diagnostics());
                self.editor.set_performance_hud(
                    performance.latest_ms,
                    performance.p95_ms,
                    performance.native_rects,
                    performance.native_textures,
                );
                true
            } else {
                self.editor.refresh_performance_hud_if_stale()
            };
            if performance_hud_rebuilt && let Some(window) = &self.window {
                window.request_redraw();
            }
            self.queue_project_autosave();
            self.sync_audio_transport();
            self.sync_monitor_decode();
        }
        self.advance_phase1_ui(frame_cpu_duration);
    }

    fn show_project_hub(&mut self) {
        if self.screen == Screen::ProjectHub {
            return;
        }
        if let Some(audio) = &self.audio_engine {
            audio.pause();
        }
        self.audio_transport = None;
        self.external_drop_batch_next = None;
        self.media_drag_pointer.reset();
        self.editor.cancel_media_drag();
        self.editor.cancel_transition_drag();
        egui::DragAndDrop::clear_payload(&self.egui_context);
        self.editor.playing = false;
        clear_text_focus_for_screen_change(&self.egui_context);
        self.screen = Screen::ProjectHub;
        let Some(window) = self.window.clone() else {
            return;
        };
        window.set_fullscreen(None);
        window.set_maximized(false);
        window.set_decorations(true);
        window.set_resizable(true);
        window.set_min_inner_size(Some(LogicalSize::new(980.0, 640.0)));
        window.set_title("Maelstrom — Project Hub");
        let _ = window.request_inner_size(LogicalSize::new(1240.0, 780.0));
        if let Some(monitor) = window.current_monitor() {
            let m = monitor.size();
            let w = window.outer_size();
            let p = monitor.position();
            window.set_outer_position(PhysicalPosition::new(
                p.x + (m.width as i32 - w.width as i32) / 2,
                p.y + (m.height as i32 - w.height as i32) / 2,
            ));
        }
        self.ensure_egui_state();
        window.request_redraw();
    }

    fn ensure_egui_state(&mut self) {
        if self.egui_state.is_some() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        self.egui_state = Some(egui_winit::State::new(
            self.egui_context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        ));
    }

    fn show_editor_screen(
        &mut self,
        project_name: String,
        language: Language,
        snapshot: Option<EditorProjectSnapshot>,
        settings: ProjectSettings,
        save_blocked: bool,
    ) {
        if let Some(audio) = &self.audio_engine {
            audio.pause();
        }
        self.audio_transport = None;
        self.reset_proxy_session();
        self.reset_media_analysis_session();
        self.project_save_blocked = save_blocked;
        self.current_project_settings = settings;
        let frame_rate = ProjectFrameRate::new(settings.fps[0], settings.fps[1])
            .unwrap_or(ProjectFrameRate::DEFAULT);
        self.editor = match snapshot {
            Some(snapshot) => {
                match EditorState::restore_with_frame_rate(
                    language,
                    project_name.clone(),
                    snapshot,
                    frame_rate,
                ) {
                    Ok(editor) => editor,
                    Err(error) => {
                        self.hub.status = Some(format!("Could not restore project: {error}"));
                        self.project_save_blocked = true;
                        EditorState::new_with_frame_rate(language, project_name.clone(), frame_rate)
                    }
                }
            }
            None => EditorState::new_with_frame_rate(language, project_name.clone(), frame_rate),
        };
        let _ = self
            .editor
            .set_project_canvas_size(settings.size[0], settings.size[1]);
        if let Some(error) = &self.audio_engine_error {
            self.editor.set_audio_output_error(error.clone());
        }
        self.last_enqueued_generation = None;
        self.autosave_schedule.clear();
        self.pending_thumbnail = None;
        self.frame_metrics = FrameMetrics::default();
        self.media_drag_pointer.reset();
        self.editor.cancel_transition_drag();
        egui::DragAndDrop::clear_payload(&self.egui_context);
        clear_text_focus_for_screen_change(&self.egui_context);
        self.apply_kraken_upscale_capability();
        self.screen = Screen::Editor;
        if self.hub_renderer.is_none()
            && let (Some(device), Some(config)) = (&self.device, &self.surface_config)
        {
            // Build native timeline pipelines at the screen boundary. This makes editor startup
            // fail fast and lets the hidden package smoke cover WGSL validation.
            self.hub_renderer = Some(HubRenderer::new(device, config.format));
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        self.ensure_egui_state();
        window.set_title(&format!("Maelstrom — {project_name}"));
        window.set_fullscreen(None);
        window.set_decorations(true);
        window.set_maximized(true);
        window.request_redraw();
    }

    fn handle_hub_action(&mut self, action: HubAction) {
        match action {
            HubAction::NewProject {
                name,
                template,
                language,
            } => {
                let project_name = if name.trim().is_empty() {
                    "Untitled Project".to_owned()
                } else {
                    name
                };
                let project_id = self
                    .hub
                    .projects
                    .iter()
                    .map(|project| project.id)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                self.hub.projects.insert(
                    0,
                    nle_ui_core::Project {
                        id: project_id,
                        name: project_name.clone(),
                        recent: "Just now".to_owned(),
                        size: "—".to_owned(),
                        thumbnail: None,
                    },
                );
                self.hub.selected = Some(project_id);
                if let Some(catalog_path) = self.catalog_path.as_deref() {
                    self.project_paths
                        .insert(project_id, project_document_path(catalog_path, project_id));
                }
                self.queue_project_catalog_save();
                self.current_project_id = Some(project_id);
                let settings = nle_ui_core::template_video_dimensions(template)
                    .map(|video| ProjectSettings {
                        fps: [video.fps, 1],
                        size: [video.width, video.height],
                    })
                    .unwrap_or_default();
                self.show_editor_screen(project_name, language, None, settings, false);
                self.queue_project_autosave();
            }
            HubAction::OpenExisting {
                project_id,
                language,
            } => {
                let (path, fallback) = if let Some(path) = self.project_paths.get(&project_id) {
                    (path.clone(), None)
                } else if let Some(catalog_path) = self.catalog_path.as_deref() {
                    (
                        project_document_path(catalog_path, project_id),
                        Some(legacy_project_document_path(catalog_path, project_id)),
                    )
                } else {
                    self.hub.status = Some("Project path is unavailable".to_owned());
                    return;
                };
                self.request_project_load(Some(project_id), path, fallback, language);
            }
            HubAction::OpenProject { language } | HubAction::Import { language } => {
                self.request_external_project_open(language)
            }
            HubAction::Export { project_id, .. } => self.request_project_export(project_id),
            HubAction::Duplicate { project_id, .. } => self.request_project_duplicate(project_id),
        }
    }

    fn request_project_load(
        &mut self,
        known_id: Option<u32>,
        path: PathBuf,
        fallback: Option<PathBuf>,
        language: Language,
    ) {
        self.hub.status = Some("Opening project…".to_owned());
        let tx = self.project_dialog_tx.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        let _ = thread::Builder::new()
            .name("maelstrom-project-reader".into())
            .spawn(move || {
                let mut document = load_project_document(&path);
                if matches!(document, Ok(None))
                    && let Some(fallback) = fallback.as_ref()
                {
                    document = load_project_document(fallback);
                }
                let file_size = fs::metadata(&path)
                    .ok()
                    .or_else(|| fallback.as_deref().and_then(|path| fs::metadata(path).ok()))
                    .map(|metadata| metadata.len());
                let _ = tx.send(ProjectDialogResult::Opened {
                    known_id,
                    path,
                    language,
                    file_size,
                    document: document.map(|value| value.map(Box::new)),
                });
                notify();
            });
    }

    fn request_external_project_open(&mut self, language: Language) {
        self.hub.status = Some("Choose a Maelstrom project…".to_owned());
        let tx = self.project_dialog_tx.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        let _ = thread::Builder::new()
            .name("maelstrom-project-open-dialog".into())
            .spawn(move || {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Open a Maelstrom project")
                    .add_filter("Maelstrom project", &["nleproj", "json"])
                    .pick_file()
                {
                    let document = load_project_document(&path);
                    let file_size = fs::metadata(&path).ok().map(|metadata| metadata.len());
                    let _ = tx.send(ProjectDialogResult::Opened {
                        known_id: None,
                        path,
                        language,
                        file_size,
                        document: document.map(|value| value.map(Box::new)),
                    });
                    notify();
                }
            });
    }

    fn complete_project_open(
        &mut self,
        known_id: Option<u32>,
        path: PathBuf,
        language: Language,
        file_size: Option<u64>,
        document: ProjectDocument,
    ) {
        let project_name = if document.project_name.trim().is_empty() {
            path.file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Untitled Project")
                .to_owned()
        } else {
            document.project_name.clone()
        };
        let project_id = known_id.unwrap_or_else(|| {
            self.hub
                .projects
                .iter()
                .map(|project| project.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        });
        if let Some(project) = self
            .hub
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.name = project_name.clone();
            project.recent = "Just now".to_owned();
            if let Some(file_size) = file_size {
                project.size = format_file_size(file_size);
            }
        } else {
            self.hub.projects.insert(
                0,
                nle_ui_core::Project {
                    id: project_id,
                    name: project_name.clone(),
                    recent: "Just now".to_owned(),
                    size: file_size
                        .map(format_file_size)
                        .unwrap_or_else(|| "0 B".to_owned()),
                    thumbnail: None,
                },
            );
        }
        self.project_paths.insert(project_id, path);
        self.current_project_id = Some(project_id);
        self.hub.selected = Some(project_id);
        self.hub.status = None;
        self.queue_project_catalog_save();
        let settings = ProjectSettings {
            fps: document.fps,
            size: document.size,
        };
        self.show_editor_screen(
            project_name,
            language,
            Some(document.snapshot),
            settings,
            false,
        );
        let used_media = self
            .editor
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .map(|clip| clip.media.0)
            .collect::<HashSet<_>>();
        let restored_media = self
            .editor
            .media
            .iter()
            .filter(|item| used_media.contains(&item.id))
            .map(|item| (item.id, item.path.clone()))
            .collect::<Vec<_>>();
        for (media_id, path) in restored_media {
            self.request_media_analysis(media_id, path);
        }
        self.queue_project_autosave();
    }

    fn project_path_for_id(&self, project_id: u32) -> Option<PathBuf> {
        self.project_paths.get(&project_id).cloned().or_else(|| {
            self.catalog_path
                .as_deref()
                .map(|catalog| project_document_path(catalog, project_id))
        })
    }

    fn request_project_export(&mut self, project_id: u32) {
        let Some(source) = self.project_path_for_id(project_id) else {
            self.hub.status = Some("Project path is unavailable".to_owned());
            return;
        };
        self.hub.status = Some("Choose where to export the project…".to_owned());
        let tx = self.project_dialog_tx.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        let suggested_name = self
            .hub
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| format!("{}.nleproj", project.name))
            .unwrap_or_else(|| "Maelstrom Project.nleproj".to_owned());
        let _ = thread::Builder::new()
            .name("maelstrom-project-export".into())
            .spawn(move || {
                let result = if let Some(destination) = rfd::FileDialog::new()
                    .set_title("Export Maelstrom project")
                    .set_file_name(&suggested_name)
                    .add_filter("Maelstrom project", &["nleproj"])
                    .save_file()
                {
                    (|| {
                        let source_document = load_project_document(&source)?
                            .ok_or_else(|| "Project document does not exist".to_owned())?;
                        let settings = ProjectSettings {
                            fps: source_document.fps,
                            size: source_document.size,
                        };
                        let exported = nle_project_io::document_for_path(
                            &destination,
                            source_document.project_name,
                            source_document.snapshot,
                            settings,
                        );
                        nle_project_io::write_document(&destination, &exported)
                            .map_err(|error| error.to_string())?;
                        Ok(destination)
                    })()
                } else {
                    return;
                };
                let _ = tx.send(ProjectDialogResult::Exported(result));
                notify();
            });
    }

    fn request_project_duplicate(&mut self, project_id: u32) {
        let (Some(source), Some(catalog_path)) = (
            self.project_path_for_id(project_id),
            self.catalog_path.as_deref(),
        ) else {
            self.hub.status = Some("Project path is unavailable".to_owned());
            return;
        };
        let new_id = self
            .hub
            .projects
            .iter()
            .map(|project| project.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let destination = project_document_path(catalog_path, new_id);
        let language = self.hub.language;
        self.hub.status = Some("Copying project…".to_owned());
        let tx = self.project_dialog_tx.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        let _ = thread::Builder::new()
            .name("maelstrom-project-copy".into())
            .spawn(move || {
                let document = (|| {
                    let source_document = load_project_document(&source)?
                        .ok_or_else(|| "Project document does not exist".to_owned())?;
                    let settings = ProjectSettings {
                        fps: source_document.fps,
                        size: source_document.size,
                    };
                    let copy_name = format!("{} Copy", source_document.project_name);
                    let copy = nle_project_io::document_for_path(
                        &destination,
                        copy_name,
                        source_document.snapshot,
                        settings,
                    );
                    nle_project_io::write_document(&destination, &copy)
                        .map_err(|error| error.to_string())?;
                    Ok(Some(copy))
                })();
                let file_size = fs::metadata(&destination)
                    .ok()
                    .map(|metadata| metadata.len());
                let _ = tx.send(ProjectDialogResult::Opened {
                    known_id: Some(new_id),
                    path: destination,
                    language,
                    file_size,
                    document: document.map(|value| value.map(Box::new)),
                });
                notify();
            });
    }

    fn poll_project_dialog(&mut self) {
        while let Ok(result) = self.project_dialog_rx.try_recv() {
            match result {
                ProjectDialogResult::Opened {
                    known_id,
                    path,
                    language,
                    file_size,
                    document: Ok(Some(document)),
                } => self.complete_project_open(known_id, path, language, file_size, *document),
                ProjectDialogResult::Opened {
                    document: Ok(None), ..
                } => self.hub.status = Some("Project document does not exist".to_owned()),
                ProjectDialogResult::Opened {
                    document: Err(error),
                    ..
                } => self.hub.status = Some(format!("Could not open project: {error}")),
                ProjectDialogResult::Exported(Ok(path)) => {
                    self.hub.status = Some(format!("Exported project to {}", path.display()))
                }
                ProjectDialogResult::Exported(Err(error)) => {
                    self.hub.status = Some(format!("Could not export project: {error}"))
                }
                ProjectDialogResult::VideoExportDestination(Some(path)) => {
                    self.start_video_export(path)
                }
                ProjectDialogResult::VideoExportDestination(None) => self.editor.set_export_idle(),
                ProjectDialogResult::KrakenUpscaleDestination(Some(path)) => {
                    self.start_kraken_upscale(path)
                }
                ProjectDialogResult::KrakenUpscaleDestination(None) => {
                    self.editor.set_kraken_upscale_idle()
                }
            }
        }
    }

    fn request_video_export(&mut self) {
        if self.export_job.is_some() {
            if self.export_project_id != self.current_project_id {
                self.editor
                    .set_export_failed("Another project is already exporting");
            }
            return;
        }
        if let Some(message) = self.editor.quick_export_block_message() {
            self.editor.set_export_failed(message);
            self.hub.status = Some(message.to_owned());
            return;
        }
        self.editor.set_export_running(0.0);
        let tx = self.project_dialog_tx.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        let file_name = format!("{}.mp4", self.editor.project_name);
        let _ = thread::Builder::new()
            .name("maelstrom-video-export-dialog".into())
            .spawn(move || {
                let destination = rfd::FileDialog::new()
                    .set_title("Quick Export — H.264 + AAC")
                    .set_file_name(&file_name)
                    .add_filter("MP4 video", &["mp4"])
                    .save_file();
                let _ = tx.send(ProjectDialogResult::VideoExportDestination(destination));
                notify();
            });
    }

    fn start_video_export(&mut self, output: PathBuf) {
        self.start_video_export_with_ffmpeg(output, bundled_media_tool("ffmpeg"));
    }

    /// Keeps the normal export path and test harness on one request construction path. The
    /// caller is responsible for supplying an executable that has already been validated.
    fn start_video_export_with_ffmpeg(&mut self, output: PathBuf, ffmpeg: PathBuf) {
        // Recheck after the native destination dialog: the user may have changed the timeline
        // while it was open. Never send unsupported graph features to the snapshot exporter.
        if let Some(message) = self.editor.quick_export_block_message() {
            self.editor.set_export_failed(message);
            self.hub.status = Some(message.to_owned());
            return;
        }
        let request = nle_export::ExportRequest {
            snapshot: self.editor.snapshot(),
            settings: self.current_project_settings,
            output,
            ffmpeg,
            encoders: preferred_h264_encoders(self.hardware_profile.as_ref()),
        };
        let notify = Arc::clone(&self.project_dialog_notify);
        match nle_export::ExportJob::start(request, move || notify()) {
            Ok(job) => {
                self.export_job = Some(job);
                self.export_project_id = self.current_project_id;
                self.editor.set_export_running(0.0);
            }
            Err(error) => self.editor.set_export_failed(error),
        }
    }

    fn poll_video_export(&mut self) {
        let mut terminal = false;
        let owns_visible_editor =
            self.export_project_id == self.current_project_id && self.screen == Screen::Editor;
        if let Some(job) = &self.export_job {
            while let Ok(event) = job.try_recv() {
                match event {
                    nle_export::ExportEvent::EncoderStarted(encoder) => {
                        observe_encoder_backend(&mut self.observed_encoder_backend, encoder);
                    }
                    nle_export::ExportEvent::Progress(progress) => {
                        if let Some(probe) = &mut self.media_acceptance_probe {
                            probe.record_export_progress();
                        }
                        if owns_visible_editor {
                            self.editor.set_export_running(progress)
                        }
                    }
                    nle_export::ExportEvent::Completed(path) => {
                        if owns_visible_editor {
                            self.editor.set_export_completed(path.clone());
                        }
                        self.hub.status = Some(format!("Video exported to {}", path.display()));
                        terminal = true;
                    }
                    nle_export::ExportEvent::Cancelled => {
                        if owns_visible_editor {
                            self.editor.set_export_idle();
                        }
                        if let Some(probe) = &mut self.media_acceptance_probe {
                            probe.record_export_cancelled();
                        }
                        self.hub.status = Some("Video export cancelled".to_owned());
                        terminal = true;
                    }
                    nle_export::ExportEvent::Failed(error) => {
                        if owns_visible_editor {
                            self.editor.set_export_failed(error.clone());
                        }
                        self.hub.status = Some(format!("Video export failed: {error}"));
                        terminal = true;
                    }
                }
            }
        }
        if terminal {
            self.export_job.take();
            self.export_project_id = None;
        }
    }

    fn apply_kraken_upscale_capability(&mut self) {
        let has_nvidia = self
            .hardware_profile
            .as_ref()
            .is_some_and(|profile| profile.has_nvidia_gpu);
        if self.hardware_profile.is_none() {
            self.editor
                .set_kraken_upscale_capability(false, "Detecting NVIDIA RTX VSR capability…");
            return;
        }
        let (ready, reason) = nle_upscale::capability(has_nvidia);
        self.editor.set_kraken_upscale_capability(ready, reason);
    }

    fn request_kraken_upscale(&mut self) {
        if !self.editor.kraken_upscale_ready {
            self.editor
                .set_kraken_upscale_failed(self.editor.kraken_upscale_reason.clone());
            return;
        }
        if self.upscale_job.is_some() {
            return;
        }
        if self.editor.kraken_source_path().is_none() {
            self.editor
                .set_kraken_upscale_failed("Select a video clip or media card");
            return;
        }
        self.editor.set_kraken_upscale_running(0.0);
        let tx = self.project_dialog_tx.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        let file_name = format!("{}-kraken-upscale.mp4", self.editor.project_name);
        let _ = thread::Builder::new()
            .name("maelstrom-kraken-upscale-dialog".into())
            .spawn(move || {
                let destination = rfd::FileDialog::new()
                    .set_title("Kraken Upscale — NVIDIA RTX VSR")
                    .set_file_name(&file_name)
                    .add_filter("MP4 video", &["mp4"])
                    .save_file();
                let _ = tx.send(ProjectDialogResult::KrakenUpscaleDestination(destination));
                notify();
            });
    }

    fn start_kraken_upscale(&mut self, output: PathBuf) {
        let Some(input) = self.editor.kraken_source_path() else {
            self.editor
                .set_kraken_upscale_failed("Select a video clip or media card");
            return;
        };
        let request = nle_upscale::UpscaleRequest {
            input,
            output,
            ffmpeg: kraken_ffmpeg(),
            quality: nle_upscale::Quality::from_u8(self.editor.kraken_upscale_quality),
            goal: nle_upscale::goal_from_index(self.editor.kraken_upscale_goal),
        };
        let notify = Arc::clone(&self.project_dialog_notify);
        match nle_upscale::UpscaleJob::start(request, move || notify()) {
            Ok(job) => {
                self.upscale_job = Some(job);
                self.editor.set_kraken_upscale_running(0.0);
            }
            Err(error) => self.editor.set_kraken_upscale_failed(error),
        }
    }

    fn cancel_kraken_upscale(&mut self) {
        if let Some(job) = &self.upscale_job {
            job.cancel();
        }
    }

    fn poll_kraken_upscale(&mut self) {
        let mut terminal = false;
        if let Some(job) = &self.upscale_job {
            while let Ok(event) = job.try_recv() {
                match event {
                    nle_upscale::UpscaleEvent::Progress(progress) => {
                        if self.screen == Screen::Editor {
                            self.editor.set_kraken_upscale_running(progress);
                        }
                    }
                    nle_upscale::UpscaleEvent::Completed(path) => {
                        if self.screen == Screen::Editor {
                            self.editor.set_kraken_upscale_completed(path.clone());
                        }
                        self.hub.status =
                            Some(format!("Kraken Upscale finished: {}", path.display()));
                        terminal = true;
                    }
                    nle_upscale::UpscaleEvent::Cancelled => {
                        if self.screen == Screen::Editor {
                            self.editor.set_kraken_upscale_idle();
                        }
                        self.hub.status = Some("Kraken Upscale cancelled".to_owned());
                        terminal = true;
                    }
                    nle_upscale::UpscaleEvent::Failed(error) => {
                        if self.screen == Screen::Editor {
                            self.editor.set_kraken_upscale_failed(error.clone());
                        }
                        self.hub.status = Some(format!("Kraken Upscale failed: {error}"));
                        terminal = true;
                    }
                }
            }
        }
        if terminal {
            self.upscale_job.take();
        }
    }

    fn request_proxy_media(&mut self, media_id: u32, requested_path: PathBuf) {
        let Some(media) = self.editor.media.iter().find(|media| media.id == media_id) else {
            return;
        };
        if media.kind != MediaKind::Video || media.path != requested_path {
            self.editor.set_proxy_media_status(
                media_id,
                ProxyMediaStatus::Failed {
                    message: proxy_text(
                        self.editor.language,
                        "The original video source changed; using original media",
                        "元の動画ソースが変更されたため、オリジナルを使用します",
                    ),
                },
            );
            return;
        }
        if let Some(active_media_id) = self.proxy_job_media_id {
            if active_media_id != media_id {
                self.editor.set_proxy_media_status(
                    media_id,
                    ProxyMediaStatus::Failed {
                        message: proxy_text(
                            self.editor.language,
                            "Another proxy is already generating",
                            "別のプロキシを生成中です",
                        ),
                    },
                );
            }
            return;
        }
        let request = nle_proxy::ProxyRequest {
            input: requested_path,
            cache_root: self.proxy_cache_root.clone(),
            ffmpeg: bundled_media_tool("ffmpeg"),
            replace_existing: true,
        };
        let notify = Arc::clone(&self.project_dialog_notify);
        match nle_proxy::ProxyJob::start(request, move || notify()) {
            Ok(job) => {
                self.proxy_job = Some(job);
                self.proxy_job_media_id = Some(media_id);
                self.editor.set_proxy_media_status(
                    media_id,
                    ProxyMediaStatus::Generating { progress: 0.0 },
                );
                self.hub.status = Some(proxy_text(
                    self.editor.language,
                    "Generating optional proxy media…",
                    "任意のプロキシメディアを生成中…",
                ));
            }
            Err(message) => self.editor.set_proxy_media_status(
                media_id,
                ProxyMediaStatus::Failed {
                    message: proxy_error_text(
                        self.editor.language,
                        "Proxy generation could not start",
                        "プロキシ生成を開始できませんでした",
                        &message,
                    ),
                },
            ),
        }
    }

    fn poll_proxy_job(&mut self) {
        let events = self
            .proxy_job
            .as_ref()
            .map(|job| {
                std::iter::from_fn(|| job.try_recv().ok()).collect::<Vec<nle_proxy::ProxyEvent>>()
            })
            .unwrap_or_default();
        if events.is_empty() {
            return;
        }
        let Some(media_id) = self.proxy_job_media_id else {
            self.proxy_job.take();
            return;
        };
        let mut terminal = false;
        for event in events {
            match event {
                nle_proxy::ProxyEvent::Progress(progress) => self
                    .editor
                    .set_proxy_media_status(media_id, ProxyMediaStatus::Generating { progress }),
                nle_proxy::ProxyEvent::Completed(artifact) => {
                    let source_is_current = self
                        .editor
                        .media
                        .iter()
                        .find(|media| media.id == media_id)
                        .is_some_and(|media| {
                            media.kind == MediaKind::Video
                                && artifact.source.matches(&media.path)
                                && artifact.path.is_file()
                        });
                    if source_is_current {
                        self.proxy_records.insert(
                            media_id,
                            ProxyRecord {
                                artifact,
                                enabled: true,
                            },
                        );
                        self.editor.set_proxy_media_status(
                            media_id,
                            ProxyMediaStatus::Ready { enabled: true },
                        );
                        self.hub.status = Some(proxy_text(
                            self.editor.language,
                            "Proxy media ready; preview is using it",
                            "プロキシメディアの準備完了。プレビューで使用中です",
                        ));
                        self.monitor_cache_epoch = self.monitor_cache_epoch.wrapping_add(1).max(1);
                    } else {
                        self.proxy_records.remove(&media_id);
                        self.editor.set_proxy_media_status(
                            media_id,
                            ProxyMediaStatus::Failed {
                                message: proxy_text(
                                    self.editor.language,
                                    "Proxy became stale; preview is using the original",
                                    "プロキシが古いため、プレビューはオリジナルを使用します",
                                ),
                            },
                        );
                    }
                    self.reconcile_proxy_records();
                    terminal = true;
                }
                nle_proxy::ProxyEvent::Cancelled => {
                    self.editor
                        .set_proxy_media_status(media_id, ProxyMediaStatus::None);
                    self.hub.status = Some(proxy_text(
                        self.editor.language,
                        "Proxy generation cancelled",
                        "プロキシ生成をキャンセルしました",
                    ));
                    terminal = true;
                }
                nle_proxy::ProxyEvent::Failed(message) => {
                    if let Some(record) = self.proxy_records.get_mut(&media_id) {
                        record.enabled = false;
                    }
                    self.editor.set_proxy_media_status(
                        media_id,
                        ProxyMediaStatus::Failed {
                            message: proxy_error_text(
                                self.editor.language,
                                "Proxy generation failed",
                                "プロキシ生成に失敗しました",
                                &message,
                            ),
                        },
                    );
                    self.hub.status = Some(match self.editor.language {
                        Language::English => format!("Proxy generation failed: {message}"),
                        Language::Japanese => format!("プロキシ生成に失敗しました: {message}"),
                    });
                    terminal = true;
                }
            }
        }
        if terminal {
            self.proxy_job.take();
            self.proxy_job_media_id = None;
            if self.screen == Screen::Editor {
                self.sync_monitor_decode();
            }
        }
    }

    fn set_proxy_media_enabled(&mut self, media_id: u32, enabled: bool) {
        let Some(record) = self.proxy_records.get_mut(&media_id) else {
            self.editor.set_proxy_media_status(
                media_id,
                ProxyMediaStatus::Failed {
                    message: proxy_text(
                        self.editor.language,
                        "Proxy is unavailable; preview is using the original",
                        "プロキシを使用できないため、プレビューはオリジナルを使用します",
                    ),
                },
            );
            return;
        };
        let original = self
            .editor
            .media
            .iter()
            .find(|media| media.id == media_id)
            .map(|media| media.path.clone());
        let usable = original.as_deref().is_some_and(|path| {
            record.artifact.path.is_file() && record.artifact.source.matches(path)
        });
        if enabled && !usable {
            self.proxy_records.remove(&media_id);
            self.editor.set_proxy_media_status(
                media_id,
                ProxyMediaStatus::Failed {
                    message: proxy_text(
                        self.editor.language,
                        "Proxy is missing or stale; preview is using the original",
                        "プロキシが見つからないか古いため、プレビューはオリジナルを使用します",
                    ),
                },
            );
        } else {
            record.enabled = enabled;
            self.editor
                .set_proxy_media_status(media_id, ProxyMediaStatus::Ready { enabled });
        }
        self.monitor_cache_epoch = self.monitor_cache_epoch.wrapping_add(1).max(1);
        if self.screen == Screen::Editor {
            self.sync_monitor_decode();
        }
    }

    fn delete_proxy_media(&mut self, media_id: u32) {
        if self.proxy_job_media_id == Some(media_id)
            && let Some(job) = &self.proxy_job
        {
            job.cancel();
        }
        if self.proxy_delete_job.is_some() {
            self.editor.set_proxy_media_status(
                media_id,
                ProxyMediaStatus::Failed {
                    message: proxy_text(
                        self.editor.language,
                        "Another proxy is already being removed",
                        "別のプロキシを削除中です",
                    ),
                },
            );
            return;
        }
        let Some(record) = self.proxy_records.get_mut(&media_id) else {
            self.editor
                .set_proxy_media_status(media_id, ProxyMediaStatus::None);
            return;
        };
        record.enabled = false;
        let path = record.artifact.path.clone();
        let notify = Arc::clone(&self.project_dialog_notify);
        match nle_proxy::ProxyDeleteJob::start(path, move || notify()) {
            Ok(job) => {
                self.proxy_delete_job = Some(job);
                self.proxy_delete_media_id = Some(media_id);
                self.editor
                    .set_proxy_media_status(media_id, ProxyMediaStatus::Deleting);
            }
            Err(message) => {
                self.editor.set_proxy_media_status(
                    media_id,
                    ProxyMediaStatus::Failed {
                        message: proxy_error_text(
                            self.editor.language,
                            "Proxy removal could not start",
                            "プロキシ削除を開始できませんでした",
                            &message,
                        ),
                    },
                );
            }
        }
        self.monitor_cache_epoch = self.monitor_cache_epoch.wrapping_add(1).max(1);
        if self.screen == Screen::Editor {
            self.sync_monitor_decode();
        }
    }

    fn poll_proxy_delete(&mut self) {
        let Some(event) = self
            .proxy_delete_job
            .as_ref()
            .and_then(|job| job.try_recv().ok())
        else {
            return;
        };
        let Some(media_id) = self.proxy_delete_media_id else {
            self.proxy_delete_job.take();
            return;
        };
        match event {
            nle_proxy::ProxyDeleteEvent::Completed => {
                self.proxy_records.remove(&media_id);
                self.editor
                    .set_proxy_media_status(media_id, ProxyMediaStatus::None);
            }
            nle_proxy::ProxyDeleteEvent::Failed(message) => {
                self.editor.set_proxy_media_status(
                    media_id,
                    ProxyMediaStatus::Failed {
                        message: proxy_error_text(
                            self.editor.language,
                            "Proxy removal failed",
                            "プロキシ削除に失敗しました",
                            &message,
                        ),
                    },
                );
            }
        }
        self.proxy_delete_job.take();
        self.proxy_delete_media_id = None;
    }

    fn reset_proxy_session(&mut self) {
        if let Some(job) = &self.proxy_job {
            job.cancel();
        }
        self.proxy_job.take();
        self.proxy_job_media_id = None;
        if let Some(job) = &self.proxy_delete_job {
            job.cancel();
        }
        self.proxy_delete_job.take();
        self.proxy_delete_media_id = None;
        self.proxy_records.clear();
    }

    /// Runs only after background cache mutation, never in monitor submission. If pruning or an
    /// external cleanup removed a derived file, preview ownership returns to the original source.
    fn reconcile_proxy_records(&mut self) {
        let stale = self
            .proxy_records
            .iter()
            .filter_map(|(&media_id, record)| {
                let original = self
                    .editor
                    .media
                    .iter()
                    .find(|media| media.id == media_id)
                    .map(|media| media.path.as_path());
                (!record.artifact.path.is_file()
                    || original.is_none_or(|path| !record.artifact.source.matches(path)))
                .then_some(media_id)
            })
            .collect::<Vec<_>>();
        if stale.is_empty() {
            return;
        }
        for media_id in stale {
            self.proxy_records.remove(&media_id);
            self.editor.set_proxy_media_status(
                media_id,
                ProxyMediaStatus::Failed {
                    message: proxy_text(
                        self.editor.language,
                        "Proxy is missing or stale; preview is using the original",
                        "プロキシが見つからないか古いため、プレビューはオリジナルを使用します",
                    ),
                },
            );
        }
        self.monitor_cache_epoch = self.monitor_cache_epoch.wrapping_add(1).max(1);
    }

    /// A derived source that cannot decode must never strand the monitor. Decoder errors are the
    /// nonblocking boundary where a concurrently deleted/corrupt proxy is retired and retried from
    /// the original; the hot monitor-submission path remains free of filesystem calls.
    fn fallback_from_failed_proxy_decode(&mut self, layer: usize) -> bool {
        let Some(identity) = self.monitor_source_identities[layer].as_ref() else {
            return false;
        };
        let Some(record) = self.proxy_records.get(&identity.media_id) else {
            return false;
        };
        if !record.enabled || identity.path != record.artifact.path {
            return false;
        }
        let media_id = identity.media_id;
        if let Some(record) = self.proxy_records.get_mut(&media_id) {
            record.enabled = false;
        }
        self.editor.set_proxy_media_status(
            media_id,
            ProxyMediaStatus::Failed {
                message: proxy_text(
                    self.editor.language,
                    "Proxy could not be decoded; preview returned to the original",
                    "プロキシをデコードできないため、プレビューをオリジナルに戻しました",
                ),
            },
        );
        self.monitor_cache_epoch = self.monitor_cache_epoch.wrapping_add(1).max(1);
        true
    }

    fn active_monitor_source_kind(&self, layer: usize, media_id: u32) -> ActivePreviewSourceKind {
        let uses_proxy = self
            .monitor_source_identities
            .get(layer)
            .and_then(Option::as_ref)
            .filter(|identity| identity.media_id == media_id)
            .and_then(|identity| {
                self.proxy_records
                    .get(&media_id)
                    .filter(|record| record.enabled && identity.path == record.artifact.path)
            })
            .is_some();
        if uses_proxy {
            ActivePreviewSourceKind::UserProxyMedia
        } else {
            ActivePreviewSourceKind::OriginalSource
        }
    }

    fn queue_project_catalog_save(&mut self) {
        let Some(path) = &self.catalog_path else {
            return;
        };
        self.catalog_writer.save_latest(CatalogSaveRequest {
            path: path.clone(),
            catalog: project_catalog_snapshot(&self.hub.projects, &self.project_paths),
        });
    }

    fn queue_project_autosave(&mut self) {
        self.queue_project_autosave_at(Instant::now(), false);
    }

    fn queue_project_autosave_immediately(&mut self) {
        self.queue_project_autosave_at(Instant::now(), true);
    }

    fn queue_project_autosave_at(&mut self, now: Instant, force: bool) {
        if self.project_save_blocked {
            return;
        }
        let Some(project_id) = self.current_project_id else {
            return;
        };
        let Some(project_path) = self.project_paths.get(&project_id).cloned().or_else(|| {
            self.catalog_path
                .as_deref()
                .map(|catalog_path| project_document_path(catalog_path, project_id))
        }) else {
            return;
        };
        let generation = self.editor.durable_generation();
        if !self.autosave_schedule.ready(
            self.last_enqueued_generation,
            generation,
            self.pending_thumbnail.is_some(),
            force,
            now,
        ) {
            return;
        }
        // This deliberately occurs only for a real persistent edit or a pending thumbnail.
        // `advance_playback` changes neither, so continuous playback never clones a project.
        let snapshot = self.editor.snapshot();
        let catalog_path = self.catalog_path.as_deref();
        let thumbnail = self.pending_thumbnail.take().and_then(|image| {
            catalog_path.map(|path| (project_thumbnail_path(path, project_id), image))
        });
        self.project_writer.save_latest(SaveRequest {
            project_path: project_path.clone(),
            document: nle_project_io::document_for_path(
                &project_path,
                self.editor.project_name.clone(),
                snapshot.clone(),
                self.current_project_settings,
            ),
            thumbnail,
        });
        self.last_enqueued_generation = Some(generation);
        self.autosave_schedule.clear();
    }

    fn poll_project_writer_events(&mut self) -> bool {
        let mut catalog_changed = false;
        while let Ok(success) = self.project_writer.success_rx.try_recv() {
            let project_id = self
                .project_paths
                .iter()
                .find_map(|(id, path)| (path == &success.project_path).then_some(*id));
            if let Some(project) = project_id.and_then(|id| {
                self.hub
                    .projects
                    .iter_mut()
                    .find(|project| project.id == id)
            }) {
                let size = format_file_size(success.file_size);
                if project.size != size {
                    project.size = size;
                    catalog_changed = true;
                }
            }
        }
        if catalog_changed {
            self.queue_project_catalog_save();
        }
        let mut had_error = false;
        while let Ok(failure) = self.project_writer.error_rx.try_recv() {
            had_error = true;
            self.hub.status = Some(format!(
                "Could not save project {}: {}",
                failure.request.project_path.display(),
                failure.message
            ));
        }
        had_error
    }

    fn poll_catalog_writer_errors(&mut self) {
        while let Ok(failure) = self.catalog_writer.error_rx.try_recv() {
            self.hub.status = Some(format!(
                "Could not save project catalog {}: {}",
                failure.request.path.display(),
                failure.message
            ));
        }
    }

    fn flush_project_autosave(&mut self) {
        self.poll_project_writer_events();
        self.queue_project_autosave_immediately();
        self.project_writer.flush();
        if self.poll_project_writer_events() {
            self.queue_project_autosave_immediately();
            self.project_writer.flush();
            self.poll_project_writer_events();
        }
    }

    fn install_project_thumbnail_textures(&mut self, thumbnails: Vec<(u32, ThumbnailRgba)>) {
        for (project_id, image) in thumbnails {
            let texture = self.egui_context.load_texture(
                format!("project-thumbnail-{project_id}"),
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width as usize, image.height as usize],
                    &image.rgba,
                ),
                egui::TextureOptions::LINEAR,
            );
            if let Some(project) = self.hub.projects.iter_mut().find(|p| p.id == project_id) {
                project.thumbnail = Some(texture.id());
            }
            self.project_thumbnail_textures.insert(project_id, texture);
        }
    }

    fn start_startup_resources(&mut self) {
        if self.startup_resources_started {
            return;
        }
        self.startup_resources_started = true;
        let Some(tx) = self.startup_resources_tx.take() else {
            return;
        };
        let catalog_path = self.catalog_path.clone();
        let fallback_catalog_path = catalog_path.clone();
        let worker_tx = tx.clone();
        let notify = Arc::clone(&self.startup_resources_notify);
        let started = thread::Builder::new()
            .name("maelstrom-startup-resources".into())
            .spawn(move || {
                let resources = load_startup_resources(catalog_path);
                let _ = worker_tx.send(resources);
                notify();
            });
        if let Err(error) = started {
            tracing::warn!(%error, "startup resource worker unavailable; loading after first frame");
            drop(tx);
            self.apply_startup_resources(load_startup_resources(fallback_catalog_path));
        }
    }

    /// Installs UI-only assets after the splash has reached the display. The splash renderer has
    /// already decoded its own GPU textures; decoding the same two large PNGs for the later Hub
    /// before the first present needlessly consumed the startup budget.
    fn initialize_hub_visuals_after_first_frame(&mut self) {
        if self.hub_backdrop_textures.is_some() {
            return;
        }
        let Some(backdrops) = self.pending_hub_backdrops.take() else {
            return;
        };
        let mut fonts = egui::FontDefinitions::default();
        configure_fonts(&mut fonts);
        self.egui_context.set_fonts(fonts);
        self.hub_backdrop_textures = Some([
            load_hub_backdrop(&self.egui_context, "hub-backdrop-english", &backdrops[0]),
            load_hub_backdrop(&self.egui_context, "hub-backdrop-japanese", &backdrops[1]),
        ]);
    }

    fn apply_startup_resources(&mut self, resources: StartupResources) {
        if let Some((projects, paths)) = resources.catalog {
            self.hub.set_projects(projects);
            self.project_paths = paths;
        }
        self.install_project_thumbnail_textures(resources.thumbnails);
        if let Some(error) = resources.thumbnail_error {
            self.hub.status = Some(error);
        }
        self.preloaded_models = resources.preloaded_models;
        tracing::info!(
            models = self.preloaded_models.len(),
            bytes = self.preloaded_models.total_bytes(),
            "startup model preload completed"
        );
        if let Some(first) = resources.model_errors.first() {
            self.hub.status = Some(if resources.model_errors.len() == 1 {
                first.clone()
            } else {
                format!(
                    "{} model preload errors; first: {first}",
                    resources.model_errors.len()
                )
            });
        }
        self.startup_resources_ready = true;
        self.refresh_app_resources_ready();
    }

    fn initialize_audio_engine_after_first_frame(&mut self) {
        if self.audio_engine_initialized {
            return;
        }
        self.audio_engine_initialized = true;
        match nle_audio::AudioEngine::new() {
            Ok(engine) => self.audio_engine = Some(engine),
            Err(error) => {
                self.audio_engine_error = Some(error.clone());
                self.editor.set_audio_output_error(error);
            }
        }
        self.refresh_app_resources_ready();
        self.sync_audio_transport();
    }

    fn poll_startup_resources(&mut self) {
        while let Ok(resources) = self.startup_resources_rx.try_recv() {
            self.apply_startup_resources(resources);
        }
    }

    fn refresh_app_resources_ready(&mut self) {
        self.app_resources_ready = startup_resources_are_ready(
            self.hardware_profile.is_some(),
            self.startup_resources_ready,
            self.audio_engine_initialized,
        );
    }

    fn handle_editor_action(&mut self, action: EditorAction) {
        match action {
            EditorAction::ChooseMediaFiles => self.request_media_files(),
            EditorAction::AnalyzeMedia { media_id, path } => {
                self.request_media_analysis(media_id, path)
            }
            EditorAction::GenerateProxyMedia { media_id, path } => {
                self.request_proxy_media(media_id, path)
            }
            EditorAction::CancelProxyMedia { media_id } => {
                if self.proxy_job_media_id == Some(media_id)
                    && let Some(job) = &self.proxy_job
                {
                    job.cancel();
                }
            }
            EditorAction::SetProxyMediaEnabled { media_id, enabled } => {
                self.set_proxy_media_enabled(media_id, enabled)
            }
            EditorAction::DeleteProxyMedia { media_id } => self.delete_proxy_media(media_id),
            EditorAction::StartExport => self.request_video_export(),
            EditorAction::CancelExport => {
                if let Some(job) = &self.export_job {
                    job.cancel();
                }
            }
            EditorAction::StartKrakenUpscale => self.request_kraken_upscale(),
            EditorAction::CancelKrakenUpscale => self.cancel_kraken_upscale(),
            EditorAction::SetForceSoftwareDecode(_) => {
                if let Some(renderer) = &mut self.hub_renderer {
                    renderer.clear_viewer_compositor();
                }
                for layer in 0..MONITOR_LAYER_COUNT {
                    // This positional slot no longer contributes. Release its sticky foreground
                    // and speculative sessions instead of letting inactive media retain global
                    // permits and starve the new top-priority source.
                    let _ = self.monitor_decoders[layer].reset_live_cache();
                    self.invalidate_monitor_request(layer);
                    self.monitor_last_requests[layer] = None;
                    self.monitor_source_identities[layer] = None;
                    self.monitor_textures[layer] = None;
                    self.monitor_last_proxy_frames[layer] = None;
                    self.monitor_requests_in_flight[layer] = false;
                    self.monitor_request_deferred[layer] = false;
                    self.monitor_request_started_at[layer] = None;
                }
                self.editor.reset_monitor();
                self.sync_monitor_decode();
            }
            #[cfg(debug_assertions)]
            EditorAction::SetVsync(enabled) => self.set_vsync(enabled),
            EditorAction::ReturnToHub => {
                self.poll_project_writer_events();
                self.queue_project_autosave_immediately();
                self.reset_media_analysis_session();
                if let Some(window) = &self.window {
                    window.set_fullscreen(None);
                    window.set_maximized(false);
                }
                self.show_project_hub();
            }
        }
    }

    fn request_media_files(&self) {
        let tx = self.media_dialog_tx.clone();
        let _ = thread::Builder::new()
            .name("maelstrom-media-file-dialog".into())
            .spawn(move || {
                let files = rfd::FileDialog::new()
                    .set_title("Import media into Maelstrom")
                    .add_filter(
                        "Media",
                        &[
                            "mp4", "mov", "mkv", "avi", "webm", "ts", "mts", "m2ts", "mp3", "wav",
                            "flac", "aac", "m4a", "png", "jpg", "jpeg", "webp", "tif", "tiff",
                            "bmp", "gif", "exr",
                        ],
                    )
                    .pick_files()
                    .unwrap_or_default();
                let _ = tx.send(files);
            });
    }

    fn poll_media_dialog(&mut self) {
        while let Ok(paths) = self.media_dialog_rx.try_recv() {
            self.add_media_paths(paths);
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Registers imports synchronously. Expensive analysis is intentionally deferred until
    /// placement emits `EditorAction::AnalyzeMedia`.
    fn add_media_paths<I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        self.editor.add_media_paths(paths);
    }

    /// Opt-in packaged acceptance path. Import begins immediately, but placement waits for a real
    /// editor layout so the package must prove the visible Media Pool drag and timeline drop
    /// geometry are connected before analysis, monitor, audio, and export can proceed.
    fn start_media_acceptance_smoke(&mut self) {
        let Some(path) = std::env::var_os("MAELSTROM_MEDIA_ACCEPTANCE_PATH").map(PathBuf::from)
        else {
            return;
        };
        self.add_media_paths([path]);
        let Some(media_id) = self.editor.media.last().map(|item| item.id) else {
            return;
        };
        self.media_acceptance_pending_drag = Some(media_id);
        self.media_acceptance_export_path =
            std::env::var_os("MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH").map(PathBuf::from);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn advance_media_acceptance_drag_smoke(&mut self) {
        let Some(media_id) = self.media_acceptance_pending_drag else {
            return;
        };
        let Some((viewer_panel_height, timeline_panel_height)) =
            self.editor.rendered_panel_heights()
        else {
            return;
        };
        if !self.editor.exercise_layout_backed_media_drop(media_id) {
            return;
        }
        self.media_acceptance_pending_drag = None;
        let (linked_video_bars, linked_audio_bars) =
            self.editor
                .timeline
                .tracks
                .iter()
                .fold((0, 0), |(video, audio), track| {
                    let bars = track
                        .clips
                        .iter()
                        .filter(|clip| clip.media.0 == media_id)
                        .count();
                    match track.kind {
                        nle_timeline::TrackKind::Video => (video + bars, audio),
                        nle_timeline::TrackKind::Audio => (video, audio + bars),
                    }
                });
        let Some(action) = self.editor.take_action() else {
            return;
        };
        self.handle_editor_action(action);
        self.editor.set_playhead(nle_timeline::Tick(0));
        self.editor.start_playback();
        self.media_acceptance_probe = MediaAcceptanceProbe::from_environment(
            media_id,
            MediaAcceptanceInitialEvidence {
                media_pool_drag_completed: true,
                viewer_panel_height,
                timeline_panel_height,
                timeline_view_span_ticks: self.editor.timeline_view_span.0,
                timeline_end_ticks: self.editor.timeline_end().0,
                linked_video_bars,
                linked_audio_bars,
            },
        );
    }

    fn maybe_start_media_acceptance_export(&mut self) {
        let analysis_ready = self.media_acceptance_probe.as_ref().is_some_and(|probe| {
            probe.analysis_metadata_ready && probe.waveform_ready && !probe.export_started
        });
        if !analysis_ready || self.export_job.is_some() {
            return;
        }
        let Some(output) = self.media_acceptance_export_path.take() else {
            return;
        };
        self.start_video_export(output);
        let export_started = self.export_job.is_some();
        if let Some(probe) = &mut self.media_acceptance_probe {
            probe.record_export_started(export_started);
        }
    }

    fn request_media_analysis(&mut self, media_id: u32, path: PathBuf) {
        let job_id = (self.media_analysis_epoch, media_id);
        if self.media_analysis_in_flight.contains(&job_id)
            || self
                .media_analysis_pending
                .iter()
                .any(|(epoch, pending_id, _)| (*epoch, *pending_id) == job_id)
        {
            return;
        }
        self.media_analysis_pending
            .push_back((self.media_analysis_epoch, media_id, path));
        self.pump_media_analysis();
    }

    fn reset_media_analysis_session(&mut self) {
        for cancel in self.media_analysis_cancellations.values() {
            cancel.store(true, Ordering::Release);
        }
        for (_, worker) in self.media_analysis_workers.drain() {
            let _ = worker.join();
        }
        self.media_analysis_epoch = self.media_analysis_epoch.wrapping_add(1);
        self.monitor_cache_epoch = self.monitor_cache_epoch.wrapping_add(1).max(1);
        self.media_analysis_pending.clear();
        self.media_analysis_in_flight.clear();
        self.media_analysis_cancellations.clear();
        self.media_acceptance_probe = None;
        self.media_acceptance_pending_drag = None;
        self.media_acceptance_export_path = None;
        self.video_strip_textures.clear();
        self.video_strips.clear();
        self.video_strip_order.clear();
        self.video_strip_bytes = 0;
        if let Some(renderer) = &mut self.hub_renderer {
            renderer.clear_timeline_textures();
            renderer.clear_viewer_compositor();
        }
        for layer in 0..MONITOR_LAYER_COUNT {
            let _ = self.monitor_decoders[layer].reset_live_cache();
            self.monitor_textures[layer] = None;
            self.monitor_last_proxy_frames[layer] = None;
            self.monitor_last_requests[layer] = None;
            self.monitor_source_identities[layer] = None;
            self.monitor_generations[layer] =
                self.monitor_generations[layer].wrapping_add(1).max(1);
            self.monitor_latest_request_ids[layer] = 0;
            self.monitor_requests_in_flight[layer] = false;
            self.monitor_request_deferred[layer] = false;
            self.monitor_request_started_at[layer] = None;
        }
        self.adaptive_preview = AdaptivePreviewController::default();
        self.editor
            .set_auto_preview_quality(self.adaptive_preview.resolved);
        self.editor.reset_monitor();
        while let Ok(result) = self.media_analysis_rx.try_recv() {
            self.media_analysis_in_flight
                .remove(&(result.project_epoch, result.media_id));
            self.media_analysis_cancellations
                .remove(&(result.project_epoch, result.media_id));
        }
    }

    fn retain_video_strip(&mut self, media_id: u32, strip: Arc<nle_waveform::VideoStrip>) {
        let bytes = strip.rgba.len();
        if bytes > MAX_RUNTIME_VIDEO_STRIP_BYTES {
            return;
        }
        if let Some(previous) = self.video_strips.remove(&media_id) {
            self.video_strip_bytes = self.video_strip_bytes.saturating_sub(previous.rgba.len());
            self.video_strip_order.retain(|id| *id != media_id);
        }
        while self.video_strips.len() >= MAX_RUNTIME_VIDEO_STRIPS
            || self.video_strip_bytes.saturating_add(bytes) > MAX_RUNTIME_VIDEO_STRIP_BYTES
        {
            let Some(oldest_media_id) = self.video_strip_order.pop_front() else {
                break;
            };
            if let Some(oldest) = self.video_strips.remove(&oldest_media_id) {
                self.video_strip_bytes = self.video_strip_bytes.saturating_sub(oldest.rgba.len());
            }
        }
        self.video_strip_bytes = self.video_strip_bytes.saturating_add(bytes);
        self.video_strips.insert(media_id, strip);
        self.video_strip_order.push_back(media_id);
    }

    fn touch_video_strip(&mut self, media_id: u32) {
        self.video_strip_order.retain(|id| *id != media_id);
        self.video_strip_order.push_back(media_id);
    }

    fn present_scrub_proxy(
        &mut self,
        layer: usize,
        media_id: u32,
        source_tick: i64,
        selected_quality: PreviewQuality,
        resolved_quality: PreviewQuality,
    ) {
        let Some(strip) = self.video_strips.get(&media_id).cloned() else {
            return;
        };
        self.touch_video_strip(media_id);
        let Some(frame_index) = nearest_video_strip_frame_index(&strip, source_tick) else {
            return;
        };
        let proxy_key = (media_id, frame_index);
        if !should_present_scrub_proxy(self.monitor_last_proxy_frames[layer], proxy_key) {
            return;
        }
        let Some(frame) = crop_video_strip_frame(&strip, frame_index) else {
            return;
        };
        let Some(sample_tick) = video_strip_sample_tick(&strip, frame_index) else {
            return;
        };
        self.present_monitor_rgba(
            layer,
            media_id,
            sample_tick,
            frame.width,
            frame.height,
            &frame.rgba,
        );
        let _ = self.editor.set_active_preview_diagnostic_for_layer(
            layer,
            ActivePreviewDiagnostic::new(
                media_id,
                ActivePreviewSourceKind::InternalScrubPreview,
                None,
                None,
                selected_quality,
                resolved_quality,
                [frame.width, frame.height],
            ),
        );
        self.monitor_last_proxy_frames[layer] = Some(proxy_key);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// HOT PATH — no IO. Publish the newest desired source tick to the sticky decoder.
    /// Each bounded layer owns an independent latest-wins worker so one slow source cannot hide
    /// a ready source. The viewer keeps each layer's last completed frame meanwhile.
    fn sync_monitor_decode(&mut self) {
        if self.editor.preview_quality() == PreviewQuality::Auto {
            self.editor
                .set_auto_preview_quality(self.adaptive_preview.resolved);
        }
        let mut preview = preview_request(&self.editor);
        if let Some(resolved) = self.adaptive_preview.sync_sources(preview.sources) {
            if self.editor.preview_quality() == PreviewQuality::Auto {
                self.editor.set_auto_preview_quality(resolved);
            }
            preview = preview_request(&self.editor);
        }
        if self.phase1_ui_probe.is_some() {
            // Explicit stress workload: Full source raster, independent of viewer panel size.
            preview.output_size = [1920, 1080];
        }
        self.submit_monitor_decode_request(preview);
    }

    /// Applies one immutable preview description. Keeping this transition separate makes the
    /// drag-quality to refinement-quality handoff deterministic and directly testable.
    fn submit_monitor_decode_request(&mut self, preview: PreviewRequest) {
        debug_assert!(
            !preview.audio_sources_truncated()
                || preview.audio_sources[MAX_PREVIEW_AUDIO_SOURCES - 1].is_some()
        );
        let [width, height] = preview.output_size;
        let acceleration = self.monitor_acceleration();
        let high_quality_scaling = self.editor.high_quality_playback();
        // Only the top visible source may fill background lanes while paused. Lower layers keep
        // their foreground decoder warm without multiplying speculative FFmpeg sessions.
        let prewarm_layer = (!self.editor.playing && !preview.is_scrubbing)
            .then(|| preview.sources.iter().rposition(Option::is_some))
            .flatten();
        // Speculative paused-preview workers are useful only while the timeline remains paused.
        // Tear them down before real-time playback or scrub work is admitted so they cannot
        // retain a background FFmpeg session alongside the foreground decode lane.
        if (self.editor.playing || preview.is_scrubbing)
            && self
                .monitor_last_requests
                .iter()
                .any(|request| request.is_some_and(|request| request.prewarm_scrub_workers))
        {
            self.release_speculative_monitor_sessions();
        }
        for layer in 0..MONITOR_LAYER_COUNT {
            self.monitor_admission_priorities[layer] = preview.sources[layer]
                .map(|source| source.priority)
                .unwrap_or(0);
        }
        // Release every no-longer-contributing positional slot before admitting any source. This
        // gives a newly visible high-priority layer a chance to take a hard coordinator permit
        // immediately instead of being deferred behind an inactive lower layer.
        for layer in 0..MONITOR_LAYER_COUNT {
            if preview.sources[layer].is_some() {
                continue;
            }
            let had_live_source = self.monitor_last_requests[layer].take().is_some()
                || self.monitor_source_identities[layer].is_some()
                || self.monitor_requests_in_flight[layer]
                || self.editor.monitor_frame_for_layer(layer).is_some();
            if had_live_source {
                let _ = self.monitor_decoders[layer].cancel_pending();
                self.invalidate_monitor_request(layer);
                self.monitor_textures[layer] = None;
                self.monitor_last_proxy_frames[layer] = None;
                self.monitor_requests_in_flight[layer] = false;
                self.monitor_request_deferred[layer] = false;
                self.monitor_request_started_at[layer] = None;
                self.editor.reset_monitor_layer(layer);
                self.clear_native_viewer_layer(layer);
            }
            // A positional slot that is no longer visible must not retain an actor lease. This
            // deliberately leaves the app-wide decoded-frame cache intact.
            if had_live_source {
                let _ = self.monitor_decoders[layer].release_live_sessions();
            }
            self.monitor_source_identities[layer] = None;
        }
        let (contributing_layers, contributing_count) =
            contributing_video_layers_by_priority(&preview.sources);
        for &layer in &contributing_layers[..contributing_count] {
            let source = preview.sources[layer]
                .expect("contributing monitor admission layer remains populated");
            let previous = self.monitor_last_requests[layer];
            // Compare the retained source identity while borrowing the timeline path. A path is
            // cloned only below, after the unchanged-key early return has decided to submit.
            let source_changed = {
                let target_path = self
                    .editor
                    .playback_targets()
                    .nth(layer)
                    .expect("resolved monitor layer remains stable during one sync")
                    .path;
                let target_path =
                    resolved_monitor_media_path(&self.proxy_records, source.media_id, target_path);
                monitor_source_identity_changed(
                    self.monitor_source_identities[layer].as_ref(),
                    source.media_id,
                    target_path,
                    acceleration,
                )
            };
            let media_changed = source_changed
                || previous.is_some_and(|previous| previous.media_id != source.media_id);
            let output_changed = previous
                .is_some_and(|previous| previous.width != width || previous.height != height);
            let scaling_changed = previous
                .is_some_and(|previous| previous.high_quality_scaling != high_quality_scaling);
            if source_changed || media_changed || output_changed || scaling_changed {
                let _ = self.monitor_decoders[layer].cancel_pending();
                self.invalidate_monitor_request(layer);
                self.monitor_requests_in_flight[layer] = false;
                self.monitor_request_deferred[layer] = false;
                self.monitor_request_started_at[layer] = None;
                self.adaptive_preview.reset_layer_samples(layer);
                self.monitor_last_proxy_frames[layer] = None;
                if source_changed || media_changed {
                    self.monitor_textures[layer] = None;
                    self.editor.reset_monitor_layer(layer);
                    self.clear_native_viewer_layer(layer);
                }
            }
            let key = MonitorRequestKey {
                project_epoch: self.monitor_generations[layer],
                media_id: source.media_id,
                source_tick: monitor_source_tick_for_preview(
                    source.source_tick,
                    source.source_frame_rate,
                ),
                width,
                height,
                is_scrubbing: preview.is_scrubbing,
                prewarm_scrub_workers: prewarm_layer == Some(layer),
                high_quality_scaling,
                selected_quality: preview.selected_quality,
                resolved_quality: preview.resolved_quality,
                source_frame_rate: source.source_frame_rate,
                source_frame_duration_tick: source.source_frame_duration_tick,
            };
            if preview.is_scrubbing && preview.resolved_quality != PreviewQuality::Full {
                let current = self
                    .editor
                    .monitor_frame_for_layer(layer)
                    .and_then(|frame| {
                        Some((
                            frame.media_id?,
                            frame.source_tick?.0,
                            frame.width,
                            frame.height,
                        ))
                    });
                let tolerance = monitor_source_frame_duration_tick(
                    source.source_frame_rate,
                    source.source_frame_duration_tick,
                );
                if !should_retain_close_full_monitor_frame(
                    current,
                    source.media_id,
                    key.source_tick,
                    (width, height),
                    tolerance,
                ) {
                    self.present_scrub_proxy(
                        layer,
                        source.media_id,
                        key.source_tick,
                        preview.selected_quality,
                        preview.resolved_quality,
                    );
                }
            } else {
                self.monitor_last_proxy_frames[layer] = None;
            }
            if self.monitor_last_requests[layer] == Some(key) {
                continue;
            }
            let request_id = self.monitor_next_request_id;
            self.monitor_next_request_id = self.monitor_next_request_id.wrapping_add(1).max(1);
            let target_path = self
                .editor
                .playback_targets()
                .nth(layer)
                .expect("resolved monitor layer remains stable during one sync")
                .path;
            let target_path =
                resolved_monitor_media_path(&self.proxy_records, source.media_id, target_path)
                    .to_path_buf();
            match self.monitor_decoders[layer].request(nle_decode::DecodeRequest {
                project_epoch: key.project_epoch,
                cache_epoch: self.monitor_cache_epoch,
                request_id,
                media_id: key.media_id,
                path: target_path.clone(),
                source_tick: key.source_tick,
                width: key.width,
                height: key.height,
                is_scrubbing: preview.is_scrubbing,
                prewarm_scrub_workers: key.prewarm_scrub_workers,
                high_quality_scaling,
                progressive_scrub_frames: progressive_scrub_frames(&preview),
                source_frame_duration_tick: monitor_source_frame_duration_tick(
                    source.source_frame_rate,
                    source.source_frame_duration_tick,
                ),
                acceleration,
            }) {
                Ok(()) => {
                    self.record_monitor_request_submission(
                        layer,
                        key,
                        MonitorSourceIdentity {
                            media_id: source.media_id,
                            path: target_path,
                            acceleration,
                        },
                        request_id,
                        false,
                    );
                }
                Err(nle_decode::DecoderClosed::SourceCapacityDeferred) => {
                    // The coordinator retained the newest request. It will be retried from the
                    // normal monitor pump after another positional source releases a group.
                    // It remains logically in flight so a retry event still matches this exact
                    // generation/request ID; it is not a decoder-unavailable error.
                    let requester_identity = MonitorSourceIdentity {
                        media_id: source.media_id,
                        path: target_path,
                        acceleration,
                    };
                    self.record_monitor_request_submission(
                        layer,
                        key,
                        requester_identity.clone(),
                        request_id,
                        true,
                    );
                    let released_speculative = self.release_speculative_monitor_sessions();
                    if released_speculative {
                        match self.monitor_decoders[layer].retry_deferred_requests() {
                            Ok(()) => self.monitor_request_deferred[layer] = false,
                            Err(nle_decode::DecoderClosed::SourceCapacityDeferred) => {}
                            Err(error) => {
                                self.monitor_runtime_metrics.record_error();
                                self.monitor_requests_in_flight[layer] = false;
                                self.monitor_request_deferred[layer] = false;
                                self.monitor_request_started_at[layer] = None;
                                self.editor.set_monitor_error(error.to_string());
                            }
                        }
                    }
                    let source_capacity_still_full = {
                        let diagnostics = self.monitor_source_coordinator.diagnostics();
                        diagnostics.live_source_groups >= diagnostics.source_group_cap
                    };
                    if self.monitor_request_deferred[layer]
                        && (!released_speculative || source_capacity_still_full)
                        && self.defer_lower_priority_monitor_group(
                            &preview,
                            layer,
                            &requester_identity,
                        )
                    {
                        match self.monitor_decoders[layer].retry_deferred_requests() {
                            Ok(()) => self.monitor_request_deferred[layer] = false,
                            Err(nle_decode::DecoderClosed::SourceCapacityDeferred) => {}
                            Err(error) => {
                                self.monitor_runtime_metrics.record_error();
                                self.monitor_requests_in_flight[layer] = false;
                                self.monitor_request_deferred[layer] = false;
                                self.monitor_request_started_at[layer] = None;
                                self.editor.set_monitor_error(error.to_string());
                            }
                        }
                    }
                }
                Err(error) => {
                    self.monitor_runtime_metrics.record_error();
                    self.monitor_request_deferred[layer] = false;
                    self.monitor_requests_in_flight[layer] = false;
                    self.monitor_request_started_at[layer] = None;
                    self.editor.set_monitor_error(error.to_string());
                }
            }
        }
    }

    /// Reclaims unprotected prewarm lanes globally before visible work is considered for
    /// eviction. Audio decoding is owned by the native audio engine and is not represented here.
    fn release_speculative_monitor_sessions(&mut self) -> bool {
        let mut released = false;
        for decoder in &self.monitor_decoders {
            match decoder.release_speculative_sessions() {
                Ok(did_release) => released |= did_release,
                Err(error) => {
                    self.monitor_runtime_metrics.record_error();
                    self.editor.set_monitor_error(error.to_string());
                }
            }
        }
        released
    }

    /// Yields one complete lower-priority visual source group. The decoder retains each victim's
    /// exact latest request, so its last presented frame remains visible and normal priority-ordered
    /// polling can resume it when capacity returns. Audio has independent ownership and is never
    /// consulted or changed here.
    fn defer_lower_priority_monitor_group(
        &mut self,
        preview: &PreviewRequest,
        requester_layer: usize,
        requester_identity: &MonitorSourceIdentity,
    ) -> bool {
        let selected = lower_priority_monitor_eviction_group(
            &preview.sources,
            &self.monitor_source_identities,
            &self.monitor_request_deferred,
            &self.monitor_latest_request_ids,
            requester_layer,
            requester_identity,
        );
        let mut yielded = false;
        for (layer, selected) in selected.iter().copied().enumerate() {
            if !selected {
                continue;
            }
            match self.monitor_decoders[layer].defer_live_sessions() {
                Ok(true) => {
                    yielded = true;
                    self.monitor_request_deferred[layer] = true;
                    self.monitor_requests_in_flight[layer] = true;
                    self.monitor_request_started_at[layer] =
                        Some((self.monitor_latest_request_ids[layer], Instant::now()));
                }
                Ok(false) => {}
                Err(error) => {
                    self.monitor_runtime_metrics.record_error();
                    self.monitor_requests_in_flight[layer] = false;
                    self.monitor_request_deferred[layer] = false;
                    self.monitor_request_started_at[layer] = None;
                    self.editor.set_monitor_error(error.to_string());
                }
            }
        }
        yielded
    }

    fn sync_audio_transport(&mut self) {
        if let Some(error) = self
            .audio_engine
            .as_ref()
            .and_then(nle_audio::AudioEngine::take_error)
        {
            self.audio_engine_error = Some(error.clone());
            self.editor.set_audio_output_error(error);
        }
        self.advance_playback_soak(Instant::now());
        let Some(audio) = &self.audio_engine else {
            self.editor.set_audio_meter_levels(0.0, 0.0);
            return;
        };
        if !self.editor.playing {
            self.editor.set_audio_meter_levels(0.0, 0.0);
            if self.audio_transport.take().is_some() {
                audio.pause();
            }
            return;
        }
        let audio_clock_tick = self.audio_transport.as_ref().and_then(|transport| {
            audio.playback_source_tick().map(|source_tick| {
                audio_master_timeline_tick(
                    transport.timeline_tick,
                    transport.source_tick,
                    source_tick,
                )
            })
        });
        if let Some(tick) = audio_clock_tick {
            self.editor
                .synchronize_playback_clock(nle_timeline::Tick(tick));
        }
        let (left, right) = audio.meter_levels();
        self.editor.set_audio_meter_levels(left, right);
        let export_active = self.export_job.is_some();
        if let Some(probe) = &mut self.media_acceptance_probe {
            probe.record_playback(self.editor.playhead.0, left, right, export_active);
            if probe.should_cancel_export()
                && let Some(job) = &self.export_job
            {
                job.cancel();
                probe.record_export_cancel_requested();
            }
        }
        let acceptance_audio_action = self.media_acceptance_probe.as_ref().and_then(|probe| {
            if probe.should_request_fade_reduction() {
                Some((probe.media_id, MediaAcceptanceAudioAction::ApplyFade))
            } else if probe.should_clear_fade() {
                Some((probe.media_id, MediaAcceptanceAudioAction::ClearFade))
            } else if probe.should_request_gain_reduction() {
                Some((probe.media_id, MediaAcceptanceAudioAction::ReduceGain))
            } else {
                None
            }
        });
        if let Some((media_id, action)) = acceptance_audio_action {
            let audio_clip = self
                .editor
                .timeline
                .tracks
                .iter()
                .filter(|track| track.kind == nle_timeline::TrackKind::Audio)
                .flat_map(|track| &track.clips)
                .find(|clip| clip.media.0 == media_id)
                .map(|clip| (clip.id, clip.duration));
            if let Some((audio_clip, duration)) = audio_clip {
                let changed = match action {
                    MediaAcceptanceAudioAction::ApplyFade => self
                        .editor
                        .timeline
                        .set_fade_duration(audio_clip, nle_timeline::FadeEdge::In, duration),
                    MediaAcceptanceAudioAction::ClearFade => {
                        self.editor.timeline.set_fade_duration(
                            audio_clip,
                            nle_timeline::FadeEdge::In,
                            nle_timeline::Tick(0),
                        )
                    }
                    MediaAcceptanceAudioAction::ReduceGain => self
                        .editor
                        .timeline
                        .set_audio_gain(audio_clip, nle_timeline::MIN_GAIN_DB),
                }
                .is_ok();
                if changed && let Some(probe) = &mut self.media_acceptance_probe {
                    match action {
                        MediaAcceptanceAudioAction::ApplyFade => {
                            probe.record_fade_reduction_requested(self.editor.playhead.0)
                        }
                        MediaAcceptanceAudioAction::ClearFade => {
                            probe.record_fade_clear_requested(self.editor.playhead.0)
                        }
                        MediaAcceptanceAudioAction::ReduceGain => {
                            probe.record_gain_reduction_requested(self.editor.playhead.0)
                        }
                    }
                }
            }
        }
        let targets = self.editor.audio_playback_targets();
        if targets.is_empty() {
            audio.pause();
            self.audio_transport = None;
            return;
        }
        let keys: Vec<_> = targets
            .iter()
            .map(|target| AudioClipKey {
                track_id: target.track_id,
                clip_id: target.clip_id,
                path: target.path.to_path_buf(),
                gain_db: target.gain_db,
                gain_left_db: target.gain_left_db,
                gain_right_db: target.gain_right_db,
                pan: target.pan,
                effects: native_audio_effects(&target.effects),
                fade_in_ticks: target.fade_in_ticks.0,
                fade_in_curve: target.fade_in_curve,
                fade_out_ticks: target.fade_out_ticks.0,
                fade_out_curve: target.fade_out_curve,
                clip_duration_ticks: target.clip_duration.0,
                transition: target.transition.map(|transition| {
                    (
                        transition.role,
                        transition.start_clip_tick.0,
                        transition.duration_ticks.0,
                    )
                }),
            })
            .collect();
        let audio_targets: Vec<_> = targets
            .iter()
            .zip(&keys)
            .map(|(target, key)| nle_audio::AudioTarget {
                track_id: key.track_id.0,
                clip_id: key.clip_id.0,
                path: key.path.clone(),
                source_tick: target.source_tick.0,
                clip_tick: target.clip_tick.0,
                gain_db: key.gain_db,
                gain_left_db: key.gain_left_db,
                gain_right_db: key.gain_right_db,
                pan: key.pan,
                effects: key.effects.clone(),
                fade_in_ticks: key.fade_in_ticks,
                fade_in_curve: key.fade_in_curve,
                fade_out_ticks: key.fade_out_ticks,
                fade_out_curve: key.fade_out_curve,
                clip_duration_ticks: key.clip_duration_ticks,
                transition: target.transition.map(|transition| {
                    nle_audio::AudioTransitionEnvelope {
                        role: match transition.role {
                            nle_ui_core::AudioPlaybackTransitionRole::Outgoing => {
                                nle_audio::AudioTransitionRole::Outgoing
                            }
                            nle_ui_core::AudioPlaybackTransitionRole::Incoming => {
                                nle_audio::AudioTransitionRole::Incoming
                            }
                        },
                        start_clip_tick: transition.start_clip_tick.0,
                        duration_ticks: transition.duration_ticks.0,
                    }
                }),
            })
            .collect();
        let target_source_ticks: Vec<_> =
            targets.iter().map(|target| target.source_tick.0).collect();
        let timeline_tick = self.editor.playhead.0;
        let continuity_tolerance = self.editor.frame_duration_tick().0.max(1);
        // Gain and fade handles are live mix controls. Preserve queued PCM and the device clock
        // when only those settings changed; restarting decode here made the control appear inert
        // during a drag and introduced avoidable gaps.
        if let Some(current) = &mut self.audio_transport
            && current.keys != keys
            && same_audio_lane_identity(&current.keys, &keys)
            && audio.update_mix_settings(&audio_targets)
        {
            current.keys.clone_from(&keys);
        }
        // Entering and leaving a crossfade changes the lane count. If a retained lane proves that
        // transport time is still continuous, add/remove only the changed lane and keep all
        // already-buffered PCM plus the native device clock. A real playhead jump still falls
        // through to the full seek below.
        if let Some(current) = &mut self.audio_transport
            && current.keys != keys
            && !same_audio_lane_identity(&current.keys, &keys)
        {
            let elapsed = current
                .started_at
                .elapsed()
                .as_micros()
                .min(i64::MAX as u128) as i64;
            if retained_audio_lanes_are_continuous(
                current,
                &keys,
                &target_source_ticks,
                elapsed,
                continuity_tolerance,
            ) && audio.reconcile_playing_targets(audio_targets.clone())
            {
                current.keys.clone_from(&keys);
                current.source_ticks.clone_from(&target_source_ticks);
                current.source_tick = audio
                    .playback_source_tick()
                    .unwrap_or(current.source_tick.saturating_add(elapsed));
                current.timeline_tick = timeline_tick;
                current.started_at = Instant::now();
                return;
            }
        }
        // The device stream owns the clock once started. Re-seek only for a clip/settings
        // change or a real playhead discontinuity; normal UI clock drift must not spawn decode
        // workers every frame.
        let needs_seek = self.audio_transport.as_ref().is_none_or(|current| {
            if current.keys != keys {
                return true;
            }
            let elapsed = current
                .started_at
                .elapsed()
                .as_micros()
                .min(i64::MAX as u128) as i64;
            let expected = audio
                .playback_source_tick()
                .unwrap_or_else(|| current.source_tick.saturating_add(elapsed));
            let elapsed_source = expected.saturating_sub(current.source_tick);
            targets
                .iter()
                .zip(&current.source_ticks)
                .any(|(target, source_tick)| {
                    target
                        .source_tick
                        .0
                        .saturating_sub(source_tick.saturating_add(elapsed_source))
                        .abs()
                        > self.editor.frame_duration_tick().0.max(1)
                })
        });
        if !needs_seek {
            return;
        }
        audio.seek_and_play_all(audio_targets);
        self.audio_transport = Some(AudioTransportState {
            keys,
            source_ticks: target_source_ticks,
            source_tick: targets[0].source_tick.0,
            timeline_tick,
            started_at: Instant::now(),
        });
    }

    /// Rejects a decode event that raced with cancellation without changing project epoch.
    fn invalidate_monitor_request(&mut self, layer: usize) {
        self.monitor_generations[layer] = self.monitor_generations[layer].wrapping_add(1).max(1);
        let invalidation_id = self.monitor_next_request_id;
        self.monitor_next_request_id = self.monitor_next_request_id.wrapping_add(1).max(1);
        self.monitor_latest_request_ids[layer] = invalidation_id;
        self.monitor_request_started_at[layer] = None;
    }

    /// Keeps a coordinator-deferred request current until its bounded retry produces the same
    /// event. This is intentionally shared with the immediate-submit path.
    fn record_monitor_request_submission(
        &mut self,
        layer: usize,
        key: MonitorRequestKey,
        source_identity: MonitorSourceIdentity,
        request_id: u64,
        deferred: bool,
    ) {
        self.monitor_runtime_metrics.record_request();
        self.monitor_last_requests[layer] = Some(key);
        self.monitor_source_identities[layer] = Some(source_identity);
        self.monitor_latest_request_ids[layer] = request_id;
        self.monitor_requests_in_flight[layer] = true;
        self.monitor_request_deferred[layer] = deferred;
        self.monitor_request_started_at[layer] = Some((request_id, Instant::now()));
    }

    fn clear_native_viewer_layer(&mut self, layer: usize) {
        if let Some(renderer) = &mut self.hub_renderer {
            let _ = renderer.clear_viewer_layer(layer);
        }
    }

    fn upload_native_viewer_layer(
        &mut self,
        layer: usize,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> bool {
        let (Some(device), Some(queue), Some(config)) = (
            self.device.clone(),
            self.queue.clone(),
            self.surface_config.clone(),
        ) else {
            return false;
        };
        let renderer = self
            .hub_renderer
            .get_or_insert_with(|| HubRenderer::new(&device, config.format));
        let started_at = Instant::now();
        match renderer.upload_viewer_layer_rgba(&device, &queue, layer, width, height, rgba) {
            Ok(()) => {
                self.viewer_upload_timings.record(started_at.elapsed());
                true
            }
            Err(error) => {
                tracing::warn!("native viewer upload fell back to egui: {error}");
                false
            }
        }
    }

    fn present_monitor_rgba(
        &mut self,
        layer: usize,
        media_id: u32,
        source_tick: i64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> bool {
        let native_uploaded = self.upload_native_viewer_layer(layer, width, height, rgba);
        let texture = self.monitor_textures[layer].get_or_insert_with(|| {
            let image = if native_uploaded {
                egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 255])
            } else {
                egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba)
            };
            self.egui_context.load_texture(
                format!("monitor-frame-{layer}"),
                image,
                egui::TextureOptions::LINEAR,
            )
        });
        if !native_uploaded {
            texture.set(
                egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba),
                egui::TextureOptions::LINEAR,
            );
        }
        self.editor.set_monitor_frame_for_layer(
            layer,
            texture.id(),
            width,
            height,
            Some(media_id),
            Some(nle_timeline::Tick(source_tick)),
        );
        native_uploaded
    }

    fn monitor_acceleration(&self) -> nle_decode::AccelerationPreference {
        if self.editor.force_software_decode {
            return nle_decode::AccelerationPreference::Software;
        }
        self.hardware_profile
            .as_ref()
            .map(|profile| {
                if profile.has_discrete_gpu
                    || profile.has_integrated_gpu
                    || profile.intel_quick_sync_candidate
                {
                    nle_decode::AccelerationPreference::PreferHardware
                } else {
                    nle_decode::AccelerationPreference::Software
                }
            })
            .unwrap_or(nle_decode::AccelerationPreference::Auto)
    }

    /// Applies one decoder event through the same generation, source, and convergence gates used
    /// by the nonblocking monitor drain. Returns whether a frame reached the retained viewer.
    fn apply_monitor_decode_event(
        &mut self,
        layer: usize,
        event: nle_decode::DecodeEvent,
        adaptive_quality_changed: &mut bool,
    ) -> bool {
        match event {
            nle_decode::DecodeEvent::Frame(frame)
                if frame.project_epoch == self.monitor_generations[layer]
                    && self
                        .editor
                        .playback_targets()
                        .nth(layer)
                        .is_some_and(|target| target.media_id == frame.media_id) =>
            {
                if !scrub_proxy_allows_monitor_frame(
                    self.monitor_last_proxy_frames[layer].is_some(),
                    self.monitor_latest_request_ids[layer],
                    frame.request_id,
                ) {
                    self.monitor_runtime_metrics.record_dropped();
                    return false;
                }
                if let Some(backend) = frame.backend {
                    let backend_name = backend.display_name();
                    if !self
                        .observed_decoder_backends
                        .iter()
                        .any(|observed| observed == backend_name)
                    {
                        self.observed_decoder_backends.push(backend_name.to_owned());
                    }
                    self.editor
                        .set_media_decoder_backend(frame.media_id, backend_name);
                }
                let target_source_tick = self.monitor_last_requests[layer]
                    .filter(|request| request.media_id == frame.media_id)
                    .map(|request| request.source_tick);
                // Progressive frames can carry the latest request ID before they reach
                // its target. Keep only this layer active until its completed frame lands.
                let latest_request_completed = monitor_frame_completes_request(
                    self.monitor_latest_request_ids[layer],
                    target_source_tick,
                    frame.request_id,
                    frame.source_tick,
                );
                self.monitor_requests_in_flight[layer] = !latest_request_completed;
                if latest_request_completed {
                    self.monitor_request_deferred[layer] = false;
                }
                let turnaround = latest_request_completed
                    .then(|| {
                        self.monitor_request_started_at[layer]
                            .take()
                            .filter(|(request_id, _)| *request_id == frame.request_id)
                            .map(|(_, started)| started.elapsed())
                    })
                    .flatten();
                if latest_request_completed {
                    self.monitor_runtime_metrics.record_completed(
                        turnaround,
                        preview_frame_budget_ms(&self.editor),
                        self.editor.monitor_frame_for_layer(layer).is_some(),
                    );
                }
                // Drag-time requests already use the dedicated scrub cap. Feeding their
                // deliberately different timings into Auto can downshift again mid-drag,
                // changing dimensions and forcing an avoidable cancel/reseek.
                if !*adaptive_quality_changed
                    && adaptive_preview_can_observe(
                        self.editor.preview_quality(),
                        self.editor.is_scrubbing(),
                    )
                    && let Some(turnaround) = turnaround
                    && let Some(resolved) = self.adaptive_preview.observe(
                        layer,
                        turnaround,
                        preview_frame_budget_ms(&self.editor),
                    )
                {
                    *adaptive_quality_changed = self.editor.set_auto_preview_quality(resolved);
                }
                let displayed_source_tick =
                    self.editor
                        .monitor_frame_for_layer(layer)
                        .and_then(|monitor| {
                            (monitor.media_id == Some(frame.media_id))
                                .then_some(monitor.source_tick)
                                .flatten()
                                .map(|tick| tick.0)
                        });
                if !target_source_tick.is_none_or(|target_source_tick| {
                    monitor_frame_converges_to_target(
                        displayed_source_tick,
                        target_source_tick,
                        frame.source_tick,
                        latest_request_completed,
                    )
                }) {
                    self.monitor_runtime_metrics.record_dropped();
                    return false;
                }
                let media_id = frame.media_id;
                let native_uploaded = self.present_monitor_rgba(
                    layer,
                    media_id,
                    frame.source_tick,
                    frame.width,
                    frame.height,
                    &frame.rgba,
                );
                let request = self.monitor_last_requests[layer]
                    .filter(|request| request.media_id == media_id);
                let decoder_backend = frame.backend.map(active_preview_decoder_backend);
                let fallback_reason = frame.fallback_reason.map(active_preview_fallback_reason);
                let source_kind = self.active_monitor_source_kind(layer, media_id);
                let _ = self.editor.set_active_preview_diagnostic_for_layer(
                    layer,
                    ActivePreviewDiagnostic::new(
                        media_id,
                        source_kind,
                        decoder_backend,
                        fallback_reason,
                        request
                            .map(|request| request.selected_quality)
                            .unwrap_or_else(|| self.editor.preview_quality()),
                        request
                            .map(|request| request.resolved_quality)
                            .unwrap_or_else(|| self.editor.resolved_preview_quality()),
                        [frame.width, frame.height],
                    ),
                );
                self.monitor_runtime_metrics
                    .record_presented(native_uploaded);
                if let Some(probe) = &mut self.phase1_ui_probe {
                    let serial = if native_uploaded {
                        self.hub_renderer
                            .as_ref()
                            .map(|renderer| {
                                renderer.viewer_presentation_evidence().upload_serials[layer]
                            })
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    probe.decoded(
                        layer,
                        media_id,
                        self.editor
                            .playback_targets()
                            .nth(layer)
                            .map(|target| target.clip_id.0)
                            .unwrap_or(0),
                        frame.project_epoch,
                        frame.request_id,
                        frame.source_tick,
                        [frame.width, frame.height],
                        frame.backend,
                        serial,
                    );
                }
                if let Some(probe) = &mut self.media_acceptance_probe {
                    probe.record_monitor_frame(media_id, native_uploaded);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                true
            }
            nle_decode::DecodeEvent::Error(error)
                if monitor_event_is_current(
                    self.monitor_generations[layer],
                    self.monitor_latest_request_ids[layer],
                    error.project_epoch,
                    error.request_id,
                ) =>
            {
                self.monitor_runtime_metrics.record_error();
                self.monitor_requests_in_flight[layer] = false;
                self.monitor_request_deferred[layer] = false;
                self.monitor_request_started_at[layer] = None;
                if self.fallback_from_failed_proxy_decode(layer) {
                    self.sync_monitor_decode();
                } else {
                    self.adaptive_preview.mark_layer_unavailable(layer);
                    self.editor.set_monitor_error(error.message);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                false
            }
            nle_decode::DecodeEvent::Frame(_) => {
                self.monitor_runtime_metrics.record_dropped();
                false
            }
            nle_decode::DecodeEvent::Error(_) => false,
        }
    }

    /// HOT PATH — nonblocking channel drains and one retained texture update per ready layer.
    fn poll_monitor_decoder(&mut self) {
        let mut adaptive_quality_changed = false;
        let deferred = self.monitor_request_deferred;
        let (retry_layers, retry_count) =
            selected_monitor_layers_by_priority(&self.monitor_admission_priorities, &deferred);
        for &layer in &retry_layers[..retry_count] {
            // Capacity deferral is retried only for a retained latest request, avoiding idle
            // endpoint locks on the ordinary monitor pump path. Priority order must match first
            // admission so a displaced lower layer cannot steal the permit back.
            match self.monitor_decoders[layer].retry_deferred_requests() {
                Ok(()) => self.monitor_request_deferred[layer] = false,
                Err(nle_decode::DecoderClosed::SourceCapacityDeferred) => {}
                Err(error) => {
                    self.monitor_runtime_metrics.record_error();
                    self.monitor_requests_in_flight[layer] = false;
                    self.monitor_request_deferred[layer] = false;
                    self.monitor_request_started_at[layer] = None;
                    self.editor.set_monitor_error(error.to_string());
                }
            }
        }
        for layer in 0..MONITOR_LAYER_COUNT {
            loop {
                let event = match self.monitor_decoders[layer].try_recv() {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => {
                        self.monitor_runtime_metrics.record_error();
                        self.editor.set_monitor_error(error.to_string());
                        break;
                    }
                };
                let _ =
                    self.apply_monitor_decode_event(layer, event, &mut adaptive_quality_changed);
            }
        }
    }

    fn pump_media_analysis(&mut self) {
        const MAX_CONCURRENT_ANALYSES: usize = 2;
        while self.media_analysis_in_flight.len() < MAX_CONCURRENT_ANALYSES {
            let Some((project_epoch, media_id, path)) = self.media_analysis_pending.pop_front()
            else {
                break;
            };
            let tx = self.media_analysis_tx.clone();
            let cancellation = Arc::new(AtomicBool::new(false));
            let worker_cancellation = Arc::clone(&cancellation);
            let started = thread::Builder::new()
                .name(format!("maelstrom-ffmpeg-analysis-{media_id}"))
                .spawn(move || {
                    let is_still = classify_path(&path) == MediaKind::Image;
                    let (metadata, frame_timing, waveform, video_strip) = if is_still {
                        match analyze_still_image(&path, &worker_cancellation) {
                            Ok(analysis) => {
                                let duration_seconds = nle_ui_core::DEFAULT_STILL_IMAGE_DURATION.0
                                    as f64
                                    / 1_000_000.0;
                                let container = path
                                    .extension()
                                    .and_then(|extension| extension.to_str())
                                    .map(|extension| extension.to_ascii_uppercase());
                                let metadata = nle_waveform::MediaMetadata {
                                    duration_seconds: Some(duration_seconds),
                                    file_size: fs::metadata(&path).ok().map(|meta| meta.len()),
                                    container: container.clone(),
                                    video_codec: container,
                                    width: Some(analysis.source_width),
                                    height: Some(analysis.source_height),
                                    streams: vec![nle_waveform::MediaStreamMetadata {
                                        index: 0,
                                        kind: Some("video".to_owned()),
                                        codec: Some("still image".to_owned()),
                                        duration_seconds: Some(duration_seconds),
                                        width: Some(analysis.source_width),
                                        height: Some(analysis.source_height),
                                        ..Default::default()
                                    }],
                                    ..Default::default()
                                };
                                (
                                    Ok(metadata),
                                    Ok(nle_waveform::FrameTiming::Unknown),
                                    Err("still images do not contain audio".to_owned()),
                                    Ok(analysis.strip),
                                )
                            }
                            Err(error) => (
                                Err(error.clone()),
                                Err(error.clone()),
                                Err(error.clone()),
                                Err(error),
                            ),
                        }
                    } else {
                        let metadata = nle_waveform::probe_media_metadata_cancellable(
                            &path,
                            Arc::clone(&worker_cancellation),
                        )
                        .map_err(|error| error.to_string());
                        let frame_timing = nle_waveform::analyze_frame_timing_cancellable(
                            &path,
                            Arc::clone(&worker_cancellation),
                        )
                        .map_err(|error| error.to_string());
                        let waveform = nle_waveform::analyze_path_cancellable(
                            &path,
                            2_048,
                            Arc::clone(&worker_cancellation),
                        )
                        .map_err(|error| error.to_string());
                        let duration = nle_waveform::probe_duration_cancellable(
                            &path,
                            Arc::clone(&worker_cancellation),
                        )
                        .map_err(|error| error.to_string());
                        let video_strip =
                            duration
                                .as_ref()
                                .map_err(Clone::clone)
                                .and_then(|duration| {
                                    nle_waveform::extract_video_strip_cancellable(
                                        &path,
                                        *duration,
                                        scrub_preview_frame_count(*duration),
                                        SCRUB_PREVIEW_FRAME_HEIGHT,
                                        Arc::clone(&worker_cancellation),
                                    )
                                    .map_err(|error| error.to_string())
                                });
                        (metadata, frame_timing, waveform, video_strip)
                    };
                    let _ = tx.send(MediaAnalysisResult {
                        project_epoch,
                        media_id,
                        is_still,
                        metadata,
                        frame_timing,
                        waveform,
                        video_strip,
                    });
                });
            if let Ok(worker) = started {
                self.media_analysis_in_flight
                    .insert((project_epoch, media_id));
                self.media_analysis_cancellations
                    .insert((project_epoch, media_id), cancellation);
                self.media_analysis_workers
                    .insert((project_epoch, media_id), worker);
            } else {
                self.editor
                    .set_waveform_error(media_id, "Could not start the FFmpeg media worker");
            }
        }
    }

    fn poll_media_analysis(&mut self) {
        while let Ok(result) = self.media_analysis_rx.try_recv() {
            let project_epoch = result.project_epoch;
            let media_id = result.media_id;
            if let Some(worker) = self
                .media_analysis_workers
                .remove(&(project_epoch, media_id))
            {
                let _ = worker.join();
            }
            self.media_analysis_in_flight
                .remove(&(project_epoch, media_id));
            self.media_analysis_cancellations
                .remove(&(project_epoch, media_id));
            if project_epoch != self.media_analysis_epoch {
                continue;
            }
            let durable_generation_before = self.editor.durable_generation();
            if let Some(probe) = &mut self.media_acceptance_probe
                && probe.media_id == media_id
            {
                probe.record_analysis(result.metadata.is_ok(), 0);
            }
            match result.metadata {
                Ok(mut metadata) => {
                    let timing_is_constant = matches!(
                        &result.frame_timing,
                        Ok(nle_waveform::FrameTiming::Constant)
                    );
                    if !timing_is_constant {
                        // An average probe rate is useful inspector text, but it is not a safe
                        // seek grid for VFR or incomplete timing scans.
                        metadata.frame_rate_ratio = None;
                        for stream in &mut metadata.streams {
                            if stream.kind.as_deref() == Some("video") {
                                stream.frame_rate_ratio = None;
                            }
                        }
                    }
                    let frame_time_index = match result.frame_timing {
                        Ok(nle_waveform::FrameTiming::Variable(index)) => {
                            nle_ui_core::SourceFrameTimeIndex::new(
                                index
                                    .into_pts()
                                    .into_iter()
                                    .map(nle_timeline::Tick)
                                    .collect(),
                            )
                        }
                        _ => None,
                    };
                    self.editor.set_media_metadata(
                        media_id,
                        nle_ui_core::MediaMetadata {
                            duration_seconds: metadata.duration_seconds,
                            file_size: metadata.file_size,
                            container: metadata.container,
                            overall_bit_rate: metadata.overall_bit_rate,
                            video_codec: metadata.video_codec,
                            width: metadata.width,
                            height: metadata.height,
                            frame_rate: metadata.frame_rate,
                            frame_rate_ratio: metadata.frame_rate_ratio.and_then(|rate| {
                                nle_ui_core::SourceFrameRate::new(
                                    rate.numerator(),
                                    rate.denominator(),
                                )
                            }),
                            video_bit_rate: metadata.video_bit_rate,
                            audio_codec: metadata.audio_codec,
                            sample_rate: metadata.sample_rate,
                            channels: metadata.channels,
                            audio_bit_rate: metadata.audio_bit_rate,
                            streams: metadata
                                .streams
                                .into_iter()
                                .map(|stream| nle_ui_core::MediaStreamMetadata {
                                    index: stream.index,
                                    kind: stream.kind,
                                    codec: stream.codec,
                                    start_seconds: stream.start_seconds,
                                    duration_seconds: stream.duration_seconds,
                                    time_base: stream.time_base,
                                    bit_rate: stream.bit_rate,
                                    width: stream.width,
                                    height: stream.height,
                                    frame_rate: stream.frame_rate,
                                    frame_rate_ratio: stream.frame_rate_ratio.and_then(|rate| {
                                        nle_ui_core::SourceFrameRate::new(
                                            rate.numerator(),
                                            rate.denominator(),
                                        )
                                    }),
                                    sample_rate: stream.sample_rate,
                                    channels: stream.channels,
                                })
                                .collect(),
                        },
                    );
                    self.editor
                        .set_media_frame_time_index(media_id, frame_time_index);
                }
                Err(error) => self.editor.set_media_error(media_id, error),
            }
            match result.waveform {
                Ok(waveform) if !result.is_still => {
                    let sample_rate = waveform.sample_rate;
                    let channels = waveform.channels;
                    let duration = waveform
                        .duration_seconds
                        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                        .map(|seconds| (seconds * 1_000_000.0).round() as i64);
                    let peaks = waveform
                        .peaks
                        .into_iter()
                        .map(|peak| (peak.min, peak.max))
                        .collect();
                    if let Some(duration) = duration {
                        if let Err(error) = self.editor.set_waveform_with_audio_info(
                            media_id,
                            nle_timeline::Tick(duration),
                            peaks,
                            sample_rate,
                            channels,
                        ) {
                            self.editor.set_waveform_error(media_id, error.to_string());
                        }
                    } else {
                        self.editor
                            .set_waveform_error(media_id, "FFmpeg did not report a duration");
                    }
                }
                Err(error) if !result.is_still => self.editor.set_waveform_error(media_id, error),
                _ => {}
            }
            if let Some(probe) = &mut self.media_acceptance_probe
                && probe.media_id == media_id
            {
                probe.record_analysis(
                    false,
                    self.editor
                        .cached_waveform(media_id)
                        .map(|waveform| waveform.peaks.len())
                        .unwrap_or(0),
                );
            }
            if let Ok(strip) = result.video_strip {
                let strip = Arc::new(strip);
                self.retain_video_strip(media_id, Arc::clone(&strip));
                let native_texture_id = timeline_texture_id(project_epoch, media_id);
                if let (Some(renderer), Some(device), Some(queue)) = (
                    self.hub_renderer.as_mut(),
                    self.device.as_ref(),
                    self.queue.as_ref(),
                ) && let Err(error) = renderer.upload_timeline_texture(
                    device,
                    queue,
                    native_texture_id,
                    strip.width,
                    strip.height,
                    &strip.rgba,
                ) {
                    self.hub.status = Some(format!(
                        "Could not upload timeline thumbnail texture: {error}"
                    ));
                    continue;
                }
                if self.pending_thumbnail.is_none() && self.current_project_id.is_some() {
                    self.pending_thumbnail = crop_representative_frame(&strip);
                    if let (Some(project_id), Some(thumbnail)) =
                        (self.current_project_id, self.pending_thumbnail.as_ref())
                    {
                        let texture = self.egui_context.load_texture(
                            format!("project-thumbnail-{project_id}"),
                            egui::ColorImage::from_rgba_unmultiplied(
                                [thumbnail.width as usize, thumbnail.height as usize],
                                &thumbnail.rgba,
                            ),
                            egui::TextureOptions::LINEAR,
                        );
                        if let Some(project) =
                            self.hub.projects.iter_mut().find(|p| p.id == project_id)
                        {
                            project.thumbnail = Some(texture.id());
                        }
                        self.project_thumbnail_textures.insert(project_id, texture);
                    }
                }
                let texture = self.egui_context.load_texture(
                    format!("timeline-video-strip-{project_epoch}-{media_id}"),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [strip.width as usize, strip.height as usize],
                        &strip.rgba,
                    ),
                    egui::TextureOptions::LINEAR,
                );
                self.editor.set_video_strip(
                    media_id,
                    native_texture_id,
                    texture.id(),
                    nle_ui_core::VideoStripLayout {
                        duration: nle_timeline::Tick(
                            (strip.duration_seconds * 1_000_000.0).round() as i64,
                        ),
                        frame_count: strip.frame_count,
                        columns: strip.columns,
                        rows: strip.rows,
                        frame_width: strip.frame_width,
                        frame_height: strip.frame_height,
                    },
                );
                self.video_strip_textures.insert(media_id, texture);
            }
            // Duration is durable project state even when an audio-only source has no video
            // strip, or when thumbnail extraction fails. Schedule it directly from the worker
            // result instead of depending on a subsequent window redraw. A new thumbnail also
            // needs an immediate save even when the scalar duration was already known.
            if self.editor.durable_generation() != durable_generation_before
                || self.pending_thumbnail.is_some()
            {
                self.queue_project_autosave();
            }
            if let Some(probe) = &mut self.media_acceptance_probe
                && probe.media_id == media_id
            {
                probe.record_resolved_timeline(
                    self.editor.timeline_view_span.0,
                    self.editor.timeline_end().0,
                );
            }
            self.maybe_start_media_acceptance_export();
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        self.pump_media_analysis();
    }

    fn start_hardware_detection(&mut self) {
        let Some(tx) = self.hardware_tx.take() else {
            return;
        };
        self.hardware_detection_started_at = Some(Instant::now());
        let started = thread::Builder::new()
            .name("maelstrom-hardware-detection".into())
            .spawn(move || {
                let profile = std::panic::catch_unwind(hardware::detect).unwrap_or_default();
                let _ = tx.send(profile);
            });
        if started.is_err() {
            self.hardware_profile = Some(HardwareProfile::default());
            self.apply_kraken_upscale_capability();
            self.refresh_app_resources_ready();
        }
    }

    fn poll_hardware_detection(&mut self) {
        if self.hardware_profile.is_some() {
            return;
        }
        const HARDWARE_DETECTION_TIMEOUT: Duration = Duration::from_secs(15);
        match self.hardware_rx.try_recv() {
            Ok(profile) => {
                self.hardware_profile = Some(profile);
                self.apply_kraken_upscale_capability();
                self.refresh_app_resources_ready();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.hardware_profile = Some(HardwareProfile::default());
                self.apply_kraken_upscale_capability();
                self.refresh_app_resources_ready();
            }
            Err(mpsc::TryRecvError::Empty)
                if self
                    .hardware_detection_started_at
                    .is_some_and(|started| started.elapsed() >= HARDWARE_DETECTION_TIMEOUT) =>
            {
                self.hardware_profile = Some(HardwareProfile::default());
                self.apply_kraken_upscale_capability();
                self.refresh_app_resources_ready();
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut window_attributes = WindowAttributes::default()
            .with_title("Maelstrom")
            .with_window_icon(Some(window_icon()))
            .with_decorations(false)
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        if std::env::var_os("MAELSTROM_SURFACE_SUBMISSION_REPORT").is_some()
            || std::env::var_os("MAELSTROM_PLAYBACK_SOAK_REPORT").is_some()
        {
            // Keep opt-in visible cadence/soak probes unobscured so Windows does not apply its
            // occluded-surface throttle. Each probe restores normal window level when complete.
            window_attributes = window_attributes.with_window_level(WindowLevel::AlwaysOnTop);
        }
        let window = Arc::new(
            event_loop
                .create_window(window_attributes)
                .expect("create splash window"),
        );
        if let Some(monitor) = window.current_monitor() {
            let monitor_size = monitor.size();
            let window_size = window.outer_size();
            let monitor_position = monitor.position();
            window.set_outer_position(PhysicalPosition::new(
                monitor_position.x + (monitor_size.width as i32 - window_size.width as i32) / 2,
                monitor_position.y + (monitor_size.height as i32 - window_size.height as i32) / 2,
            ));
        }
        self.create_gpu(window);
        if self.phase1_ui_probe.is_some() {
            self.start_phase1_ui();
        } else if std::env::var("MAELSTROM_SMOKE_EDITOR").as_deref() == Ok("1") {
            self.show_editor_screen(
                "Package Smoke".to_owned(),
                Language::English,
                None,
                ProjectSettings::default(),
                true,
            );
            self.start_media_acceptance_smoke();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.flush_project_autosave();
                event_loop.exit()
            }
            WindowEvent::KeyboardInput { event, .. }
                if self.screen == Screen::Splash
                    && event.state.is_pressed()
                    && matches!(event.logical_key, Key::Named(NamedKey::Escape)) =>
            {
                event_loop.exit()
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.screen == Screen::Splash && self.splash_can_continue() => {
                self.show_project_hub()
            }
            event @ WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.screen == Screen::Editor => {
                let cursor = self
                    .window
                    .as_deref()
                    .and_then(|window| current_cursor_in_egui_points(window, &self.egui_context));
                let claimed = self
                    .media_drag_pointer
                    .primary_pressed(cursor)
                    .is_some_and(|point| self.editor.claim_media_drag_at(point));
                if claimed {
                    self.media_drag_pointer.media_drag_claimed();
                    self.editor.cancel_transition_drag();
                    egui::DragAndDrop::clear_payload(&self.egui_context);
                }
                if let Some((state, window)) = self.egui_state.as_mut().zip(self.window.as_ref()) {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
            event @ WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } if self.screen == Screen::Editor => {
                let cached_cursor = self.media_drag_pointer.primary_released();
                let current_cursor = self
                    .window
                    .as_deref()
                    .and_then(|window| current_cursor_in_egui_points(window, &self.egui_context));
                let release_points = current_cursor
                    .into_iter()
                    .chain(cached_cursor)
                    .collect::<Vec<_>>();
                let handled = if release_points.is_empty() {
                    self.editor.cancel_media_drag()
                } else {
                    self.editor.complete_media_drag_at_any(release_points)
                };
                if handled {
                    // The native completion already inserted (or cancelled) this drag. Prevent
                    // egui's release handling from consuming its independently retained payload.
                    self.editor.cancel_transition_drag();
                    egui::DragAndDrop::clear_payload(&self.egui_context);
                    if let Some(action) = self.editor.take_action() {
                        self.handle_editor_action(action);
                    }
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                if let Some((state, window)) = self.egui_state.as_mut().zip(self.window.as_ref()) {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
            event @ WindowEvent::Focused(false) if self.screen == Screen::Editor => {
                self.media_drag_pointer.reset();
                self.editor_modifiers = ModifiersState::default();
                self.editor.cancel_media_drag();
                self.editor.cancel_transition_drag();
                egui::DragAndDrop::clear_payload(&self.egui_context);
                if let Some((state, window)) = self.egui_state.as_mut().zip(self.window.as_ref()) {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
            event @ WindowEvent::CursorMoved { position, .. } if self.screen == Screen::Editor => {
                let cursor = self.window.as_ref().map(|window| {
                    let logical = position.to_logical::<f32>(window.scale_factor());
                    egui_point_from_winit_logical(
                        egui::Pos2::new(logical.x, logical.y),
                        window.scale_factor(),
                        self.egui_context.pixels_per_point(),
                    )
                });
                let claimed = cursor.is_some_and(|point| {
                    self.media_drag_pointer
                        .cursor_moved(point)
                        .is_some_and(|point| self.editor.claim_media_drag_at(point))
                });
                if claimed {
                    self.media_drag_pointer.media_drag_claimed();
                    self.editor.cancel_transition_drag();
                    egui::DragAndDrop::clear_payload(&self.egui_context);
                }
                if let Some((state, window)) = self.egui_state.as_mut().zip(self.window.as_ref()) {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
            event @ WindowEvent::ModifiersChanged(modifiers) => {
                self.editor_modifiers = modifiers.state();
                if self.screen != Screen::Splash
                    && let Some((state, window)) =
                        self.egui_state.as_mut().zip(self.window.as_ref())
                {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
            event @ WindowEvent::KeyboardInput { .. } if self.screen == Screen::Editor => {
                let WindowEvent::KeyboardInput {
                    event: key_event, ..
                } = &event
                else {
                    unreachable!()
                };
                let mut handled = false;
                if key_event.state.is_pressed()
                    && !key_event.repeat
                    && !self.egui_context.egui_wants_keyboard_input()
                {
                    if matches!(key_event.logical_key, Key::Named(NamedKey::Space)) {
                        self.editor.toggle_playback();
                        self.sync_audio_transport();
                        self.sync_monitor_decode();
                        handled = true;
                    } else if let Some(shortcut) =
                        native_editor_shortcut(&key_event.logical_key, self.editor_modifiers)
                    {
                        match shortcut {
                            NativeEditorShortcut::Undo => {
                                self.editor.undo_timeline();
                            }
                            NativeEditorShortcut::Redo => {
                                self.editor.redo_timeline();
                            }
                            NativeEditorShortcut::Razor => {
                                self.editor.razor_at_playhead();
                            }
                            NativeEditorShortcut::Delete => {
                                self.editor.delete_selected_timeline_clip();
                            }
                            NativeEditorShortcut::CommandPalette => {
                                self.editor.open_command_palette();
                            }
                        }
                        handled = true;
                    }
                }
                if handled {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                } else if let Some((state, window)) =
                    self.egui_state.as_mut().zip(self.window.as_ref())
                {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::DroppedFile(path) if self.screen == Screen::Editor => {
                let timeline_start = self.external_drop_batch_next.or_else(|| {
                    self.window
                        .as_deref()
                        .and_then(|window| {
                            current_cursor_in_egui_points(window, &self.egui_context)
                        })
                        .and_then(|point| self.editor.timeline_drop_start_at(point))
                });
                self.editor.set_drop_hovered(false);
                self.add_media_paths([path.clone()]);
                if let Some(start) = timeline_start
                    && let Some(media_id) = self
                        .editor
                        .media
                        .iter()
                        .find(|item| item.path == path)
                        .map(|item| item.id)
                    && self.editor.overwrite_media_at(media_id, start)
                {
                    self.external_drop_batch_next = self
                        .editor
                        .selected_timeline_clip
                        .and_then(|clip_id| self.editor.timeline.clip(clip_id))
                        .map(|clip| clip.end());
                    if let Some(action) = self.editor.take_action() {
                        self.handle_editor_action(action);
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::HoveredFile(_) if self.screen == Screen::Editor => {
                self.external_drop_batch_next = None;
                self.editor.set_drop_hovered(true);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::HoveredFileCancelled if self.screen == Screen::Editor => {
                self.external_drop_batch_next = None;
                self.editor.set_drop_hovered(false);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Resized(size) => self.resize(size.width, size.height),
            WindowEvent::RedrawRequested => {
                self.render();
                self.external_drop_batch_next = None;
            }
            event => {
                if self.screen != Screen::Splash
                    && let Some((state, window)) =
                        self.egui_state.as_mut().zip(self.window.as_ref())
                {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        window.request_redraw();
                    }
                }
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Monitor => self.poll_monitor_decoder(),
            AppEvent::ProjectWriter => {
                self.poll_project_writer_events();
                self.poll_catalog_writer_errors();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AppEvent::ProjectDialog => {
                self.poll_project_dialog();
                self.poll_video_export();
                self.poll_kraken_upscale();
                self.poll_proxy_job();
                self.poll_proxy_delete();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AppEvent::StartupResources => {
                self.poll_startup_resources();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .phase1_ui_probe
            .as_ref()
            .is_some_and(|probe| probe.should_exit())
        {
            event_loop.exit();
            return;
        }
        if self.first_surface_presented {
            // Project IO remains on a worker. CPAL streams are not Send on every supported
            // platform, so native audio negotiation stays on the owner thread but begins only
            // after the splash has been presented and outside the render hot path.
            self.start_startup_resources();
            self.start_hardware_detection();
            self.initialize_audio_engine_after_first_frame();
            self.initialize_hub_visuals_after_first_frame();
        }
        self.poll_project_writer_events();
        self.poll_catalog_writer_errors();
        self.poll_project_dialog();
        self.poll_video_export();
        self.poll_kraken_upscale();
        self.poll_proxy_job();
        self.poll_proxy_delete();
        self.poll_media_dialog();
        self.poll_media_analysis();
        self.poll_monitor_decoder();
        self.poll_startup_resources();
        self.poll_hardware_detection();
        let now = Instant::now();
        if let Some(deadline) = self.autosave_schedule.deadline() {
            if now >= deadline {
                self.queue_project_autosave_at(now, false);
                // One redraw lets egui settle any final drag state after the quiet period.
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            } else if !(self.screen == Screen::Editor && self.editor.playing)
                && self.screen != Screen::Splash
            {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
        }
        if self.screen == Screen::Editor
            && self.editor.playing
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
        if self.screen == Screen::Splash {
            if self.splash_can_continue() {
                self.show_splash_continue_affordance();
                self.show_project_hub();
            } else if let Some(window) = &self.window {
                window.request_redraw();
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for cancel in self.media_analysis_cancellations.values() {
            cancel.store(true, Ordering::Release);
        }
        for (_, worker) in self.media_analysis_workers.drain() {
            let _ = worker.join();
        }
        self.flush_project_autosave();
        self.project_writer.flush_and_shutdown();
        self.catalog_writer.flush_and_shutdown();
    }
}

impl App {
    fn splash_can_continue(&self) -> bool {
        let visible_for = self
            .splash_first_presented_at
            .map(|presented_at| presented_at.elapsed())
            .unwrap_or_default();
        splash_can_continue(self.app_resources_ready, visible_for)
    }

    fn show_splash_continue_affordance(&mut self) {
        if self.splash_continue_available {
            return;
        }
        self.splash_continue_available = true;
        if let Some(window) = &self.window {
            window.set_cursor(CursorIcon::Pointer);
            window.set_title("Maelstrom");
        }
    }

    fn paint_splash_loading_overlay(&mut self, view: &wgpu::TextureView) {
        self.ensure_egui_state();
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(config) = self.surface_config.clone() else {
            return;
        };
        let Some(device) = self.device.clone() else {
            return;
        };
        let Some(queue) = self.queue.clone() else {
            return;
        };
        let Some(state) = self.egui_state.as_mut() else {
            return;
        };
        let raw_input = state.take_egui_input(window.as_ref());
        let stage = splash_load_stage(
            self.hardware_profile.is_some(),
            self.startup_resources_ready,
            self.audio_engine_initialized,
            self.splash_can_continue(),
        );
        let language = self.hub.language;
        let context = self.egui_context.clone();
        let output = context.run_ui(raw_input, |ui| {
            paint_splash_loading_line(ui, language, stage);
        });
        if let Some(state) = self.egui_state.as_mut() {
            state.handle_platform_output(window.as_ref(), output.platform_output);
        }
        let primitives = context.tessellate(output.shapes, output.pixels_per_point);
        let hub_renderer = self
            .hub_renderer
            .get_or_insert_with(|| HubRenderer::new(&device, config.format));
        hub_renderer.render_overlay(
            &device,
            &queue,
            view,
            &primitives,
            &output.textures_delta,
            egui_wgpu::ScreenDescriptor {
                size_in_pixels: [config.width, config.height],
                pixels_per_point: output.pixels_per_point,
            },
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplashLoadStage {
    Graphics,
    Library,
    Audio,
    Holding,
    Ready,
}

fn splash_load_stage(
    hardware_ready: bool,
    library_ready: bool,
    audio_ready: bool,
    can_continue: bool,
) -> SplashLoadStage {
    if !hardware_ready {
        SplashLoadStage::Graphics
    } else if !library_ready {
        SplashLoadStage::Library
    } else if !audio_ready {
        SplashLoadStage::Audio
    } else if !can_continue {
        SplashLoadStage::Holding
    } else {
        SplashLoadStage::Ready
    }
}

fn splash_load_copy(language: Language, stage: SplashLoadStage) -> &'static str {
    match (language, stage) {
        (Language::English, SplashLoadStage::Graphics) => "LOADING  GRAPHICS",
        (Language::English, SplashLoadStage::Library) => "LOADING  PROJECT LIBRARY",
        (Language::English, SplashLoadStage::Audio) => "LOADING  AUDIO ENGINE",
        (Language::English, SplashLoadStage::Holding) => "LOADING",
        (Language::English, SplashLoadStage::Ready) => "LOADING  EDITOR",
        (Language::Japanese, SplashLoadStage::Graphics) => "読み込み中　グラフィックス",
        (Language::Japanese, SplashLoadStage::Library) => "読み込み中　プロジェクト",
        (Language::Japanese, SplashLoadStage::Audio) => "読み込み中　オーディオエンジン",
        (Language::Japanese, SplashLoadStage::Holding) => "読み込み中",
        (Language::Japanese, SplashLoadStage::Ready) => "読み込み中　エディター",
    }
}

fn paint_splash_loading_line(ui: &mut egui::Ui, language: Language, stage: SplashLoadStage) {
    let screen = ui.ctx().content_rect();
    let label = splash_load_copy(language, stage);
    egui::Area::new(egui::Id::new("splash-loading-line"))
        .fixed_pos(egui::pos2(48.0, screen.bottom() - 56.0))
        .interactable(false)
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            ui.label(
                egui::RichText::new(label)
                    .size(15.0)
                    .color(egui::Color32::from_rgb(168, 188, 204)),
            );
        });
}

fn splash_can_continue(resources_ready: bool, visible_for: Duration) -> bool {
    resources_ready && visible_for >= MIN_SPLASH_VISIBLE
}

fn startup_resources_are_ready(
    hardware_ready: bool,
    catalog_ready: bool,
    audio_ready: bool,
) -> bool {
    hardware_ready && catalog_ready && audio_ready
}

fn monitor_event_is_current(
    current_epoch: u64,
    latest_request_id: u64,
    event_epoch: u64,
    event_request_id: u64,
) -> bool {
    event_epoch == current_epoch && event_request_id == latest_request_id
}

fn monitor_frame_completes_request(
    latest_request_id: u64,
    target_source_tick: Option<i64>,
    candidate_request_id: u64,
    candidate_source_tick: i64,
) -> bool {
    candidate_request_id == latest_request_id
        && target_source_tick.is_none_or(|target| {
            nle_decode::source_tick_reaches_target(candidate_source_tick, target)
        })
}

/// Allows only monitor frames that converge on the latest scrub target.
fn monitor_frame_converges_to_target(
    displayed_source_tick: Option<i64>,
    target_source_tick: i64,
    candidate_source_tick: i64,
    latest_request_completed: bool,
) -> bool {
    latest_request_completed
        || candidate_source_tick == target_source_tick
        || displayed_source_tick.is_none_or(|displayed_source_tick| {
            candidate_source_tick.abs_diff(target_source_tick)
                < displayed_source_tick.abs_diff(target_source_tick)
        })
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(if cfg!(debug_assertions) {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .with_target(true)
        .try_init();
    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("create event loop");
    let proxy = event_loop.create_proxy();
    let mut app = App::new(move |event| {
        let _ = proxy.send_event(event);
    });
    event_loop.run_app(&mut app).expect("run application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn optional_proxy_routing_changes_monitor_video_only() {
        let original = PathBuf::from("C:/media/original.mp4");
        let proxy = PathBuf::from("C:/cache/proxy.mp4");
        let mut records = HashMap::new();
        assert_eq!(
            resolved_monitor_media_path(&records, 1, &original),
            original
        );

        records.insert(
            1,
            ProxyRecord {
                artifact: nle_proxy::ProxyArtifact {
                    path: proxy.clone(),
                    source: nle_proxy::SourceFingerprint {
                        canonical_path: original.clone(),
                        bytes: 1,
                        modified_unix_nanos: 1,
                    },
                    output_bytes: 1,
                    profile_version: nle_proxy::PROXY_PROFILE_VERSION,
                },
                enabled: true,
            },
        );
        assert_eq!(resolved_monitor_media_path(&records, 1, &original), proxy);
        records.get_mut(&1).expect("proxy record").enabled = false;
        assert_eq!(
            resolved_monitor_media_path(&records, 1, &original),
            original
        );

        let mut editor = EditorState::new(Language::English, "Proxy isolation");
        editor.add_media_paths([original.clone()]);
        assert!(editor.insert_media_at(1, nle_timeline::Tick(0)));
        let audio_targets = editor.audio_playback_targets();
        assert!(!audio_targets.is_empty());
        assert!(audio_targets.iter().all(|target| target.path == original));
        assert_eq!(editor.snapshot().media[0].path, original);
    }

    #[test]
    fn proxy_route_changes_advance_cache_namespace_and_decode_error_falls_back() {
        let catalog = test_catalog_path("proxy-route-epoch");
        let root = catalog.parent().expect("test root");
        fs::create_dir_all(root).expect("create proxy test root");
        let original = root.join("original.mp4");
        let proxy = root.join("proxy.mp4");
        fs::write(&original, b"original").expect("write source");
        fs::write(&proxy, b"proxy").expect("write proxy");

        let mut app = App::new_without_startup_or_audio_for_monitor_contract();
        app.editor.add_media_paths([original.clone()]);
        app.proxy_records.insert(
            1,
            ProxyRecord {
                artifact: nle_proxy::ProxyArtifact {
                    path: proxy.clone(),
                    source: nle_proxy::SourceFingerprint::capture(&original)
                        .expect("source fingerprint"),
                    output_bytes: fs::metadata(&proxy).expect("proxy metadata").len(),
                    profile_version: nle_proxy::PROXY_PROFILE_VERSION,
                },
                enabled: false,
            },
        );
        app.editor
            .set_proxy_media_status(1, ProxyMediaStatus::Ready { enabled: false });

        let initial_epoch = app.monitor_cache_epoch;
        app.set_proxy_media_enabled(1, true);
        assert!(app.monitor_cache_epoch > initial_epoch);
        assert_eq!(
            resolved_monitor_media_path(&app.proxy_records, 1, &original),
            proxy
        );
        app.monitor_source_identities[0] = Some(MonitorSourceIdentity {
            media_id: 1,
            path: proxy.clone(),
            acceleration: nle_decode::AccelerationPreference::Auto,
        });
        assert_eq!(
            app.active_monitor_source_kind(0, 1),
            ActivePreviewSourceKind::UserProxyMedia
        );

        let enabled_epoch = app.monitor_cache_epoch;
        fs::remove_file(&proxy).expect("simulate external proxy cleanup");
        assert!(app.fallback_from_failed_proxy_decode(0));
        assert!(app.monitor_cache_epoch > enabled_epoch);
        assert_eq!(
            resolved_monitor_media_path(&app.proxy_records, 1, &original),
            original
        );
        assert!(!app.proxy_records.get(&1).expect("cleanup record").enabled);
        assert!(!proxy.exists());
        assert!(matches!(
            app.editor.proxy_media_status(1),
            ProxyMediaStatus::Failed { .. }
        ));

        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_background_proxy_delete_retains_cleanup_handle_for_retry() {
        let catalog = test_catalog_path("proxy-delete-retry");
        let root = catalog.parent().expect("test root");
        fs::create_dir_all(root).expect("create proxy delete test root");
        let original = root.join("original.mp4");
        let locked_proxy = root.join("locked-proxy.mp4");
        fs::write(&original, b"original").expect("write source");
        fs::create_dir(&locked_proxy).expect("directory cannot be removed as a file");

        let mut app = App::new_without_startup_or_audio_for_monitor_contract();
        app.editor.add_media_paths([original.clone()]);
        app.proxy_records.insert(
            1,
            ProxyRecord {
                artifact: nle_proxy::ProxyArtifact {
                    path: locked_proxy.clone(),
                    source: nle_proxy::SourceFingerprint::capture(&original)
                        .expect("source fingerprint"),
                    output_bytes: 0,
                    profile_version: nle_proxy::PROXY_PROFILE_VERSION,
                },
                enabled: false,
            },
        );
        app.editor.set_proxy_media_status(
            1,
            ProxyMediaStatus::Failed {
                message: "retry".into(),
            },
        );

        app.delete_proxy_media(1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.proxy_delete_job.is_some() && Instant::now() < deadline {
            app.poll_proxy_delete();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(app.proxy_delete_job.is_none());
        assert!(app.proxy_records.contains_key(&1));
        assert!(matches!(
            app.editor.proxy_media_status(1),
            ProxyMediaStatus::Failed { .. }
        ));

        fs::remove_dir(&locked_proxy).expect("release simulated lock");
        fs::write(&locked_proxy, b"stale proxy").expect("replace with removable stale file");
        app.delete_proxy_media(1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.proxy_delete_job.is_some() && Instant::now() < deadline {
            app.poll_proxy_delete();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(app.proxy_delete_job.is_none());
        assert!(!app.proxy_records.contains_key(&1));
        assert!(!locked_proxy.exists());
        assert_eq!(app.editor.proxy_media_status(1), ProxyMediaStatus::None);

        drop(app);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn phase0_surface_adapter_class_parses_supported_values() {
        assert_eq!(
            parse_phase0_surface_adapter_class("IntegratedGpu"),
            Ok(Phase0SurfaceAdapterClass::IntegratedGpu)
        );
        assert_eq!(
            parse_phase0_surface_adapter_class("DiscreteGpu"),
            Ok(Phase0SurfaceAdapterClass::DiscreteGpu)
        );
        assert_eq!(
            parse_phase0_surface_adapter_class("Cpu"),
            Err(
                "MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS must be IntegratedGpu or DiscreteGpu, got \"Cpu\""
                    .to_owned()
            )
        );
    }

    #[test]
    fn native_viewer_maps_basic_correction_whites_and_blacks() {
        let correction =
            viewer_color_correction(nle_timeline::EvaluatedVideoEffect::BrightnessContrast(
                nle_timeline::EvaluatedBrightnessContrast {
                    whites: 0.45,
                    blacks: -0.35,
                    ..Default::default()
                },
            ));
        assert_eq!([correction.whites, correction.blacks], [0.45, -0.35]);
    }

    #[test]
    fn native_viewer_maps_vignette_to_an_identity_curve_effect_slot() {
        let correction = viewer_color_correction(nle_timeline::EvaluatedVideoEffect::Vignette(
            nle_timeline::EvaluatedVignette {
                amount: 0.8,
                midpoint: 0.3,
                feather: 0.6,
                center_x: -0.25,
                center_y: 0.5,
            },
        ));
        assert_eq!(
            [
                correction.vignette_amount,
                correction.vignette_midpoint,
                correction.vignette_feather,
                correction.vignette_center_x,
                correction.vignette_center_y,
            ],
            [0.8, 0.3, 0.6, -0.25, 0.5]
        );
        assert_eq!(correction.curves, ViewerRgbCurves::default());
    }

    #[test]
    fn screen_change_releases_stale_text_edit_keyboard_focus() {
        let context = egui::Context::default();
        let search_id = egui::Id::new("project-hub-search");
        context.memory_mut(|memory| memory.request_focus(search_id));
        assert_eq!(context.memory(|memory| memory.focused()), Some(search_id));

        clear_text_focus_for_screen_change(&context);

        assert_eq!(context.memory(|memory| memory.focused()), None);
    }

    #[test]
    fn native_editor_shortcuts_map_windows_and_macos_primary_modifiers() {
        let ctrl = ModifiersState::CONTROL;
        let command = ModifiersState::SUPER;
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let character = |value: &'static str| Key::Character(value.into());

        assert_eq!(
            native_editor_shortcut(&character("z"), ctrl),
            Some(NativeEditorShortcut::Undo)
        );
        assert_eq!(
            native_editor_shortcut(&character("Z"), ctrl_shift),
            Some(NativeEditorShortcut::Redo)
        );
        assert_eq!(
            native_editor_shortcut(&character("y"), ctrl),
            Some(NativeEditorShortcut::Redo)
        );
        assert_eq!(
            native_editor_shortcut(&character("p"), command),
            Some(NativeEditorShortcut::CommandPalette)
        );
        assert_eq!(
            native_editor_shortcut(&Key::Named(NamedKey::Delete), ModifiersState::default()),
            Some(NativeEditorShortcut::Delete)
        );
        assert_eq!(
            native_editor_shortcut(&character("z"), ModifiersState::default()),
            None
        );
    }

    #[test]
    fn frame_metrics_publish_bounded_rolling_p95_without_heap_growth() {
        let mut metrics = FrameMetrics::default();
        let mut published = None;
        for milliseconds in 1..=16_u64 {
            if let Some(snapshot) =
                metrics.record(Duration::from_micros(milliseconds * 1_000), 42, 7)
            {
                published = Some(snapshot);
            }
        }
        let published = published.expect("initial and periodic metrics publish");
        assert_eq!(published.latest_ms, 16.0);
        assert_eq!(published.p95_ms, 16.0);
        assert_eq!((published.native_rects, published.native_textures), (42, 7));
        assert_eq!(metrics.sample_count, 16);
    }

    #[test]
    fn frame_metrics_uses_nearest_rank_p95_for_full_window() {
        let mut metrics = FrameMetrics {
            frames_since_publish: 0,
            ..Default::default()
        };
        let mut published = None;
        for milliseconds in 1..=FRAME_TIME_SAMPLE_COUNT as u64 {
            if let Some(snapshot) =
                metrics.record(Duration::from_micros(milliseconds * 1_000), 1, 1)
            {
                published = Some(snapshot);
            }
        }
        let published = published.expect("120 samples include a publish boundary");
        assert_eq!(published.latest_ms, 120.0);
        assert_eq!(published.p95_ms, 114.0);
    }

    #[test]
    fn monitor_runtime_metrics_publish_bounded_session_diagnostics() {
        let mut metrics = MonitorRuntimeMetrics::default();
        metrics.record_request();
        metrics.record_request();
        metrics.record_completed(Some(Duration::from_millis(10)), 15.0, true);
        metrics.record_completed(Some(Duration::from_millis(20)), 15.0, false);
        metrics.record_completed(Some(Duration::from_millis(30)), 15.0, true);
        metrics.record_presented(true);
        metrics.record_presented(false);
        metrics.record_dropped();
        metrics.record_error();

        assert_eq!(
            metrics.diagnostics(480, 2, 24),
            RuntimeDiagnostics {
                monitor_requests: 2,
                monitor_completed_frames: 3,
                monitor_presented_frames: 2,
                monitor_dropped_frames: 1,
                monitor_hold_events: 1,
                monitor_late_frames: 2,
                monitor_errors: 1,
                monitor_turnaround_p95_ms: 30.0,
                native_viewer_uploads: 1,
                fallback_viewer_uploads: 1,
                audio_underrun_frames: 480,
                audio_callback_lock_failures: 2,
                audio_late_discarded_frames: 24,
                live_pipeline_timing: LivePipelineTiming::default(),
            }
        );
        assert_eq!(metrics.turnaround_count, 3);

        let mut wrapped = MonitorRuntimeMetrics::default();
        for milliseconds in 0..=FRAME_TIME_SAMPLE_COUNT {
            wrapped.record_completed(
                Some(Duration::from_millis(milliseconds as u64)),
                f32::MAX,
                false,
            );
        }
        assert_eq!(wrapped.turnaround_count, FRAME_TIME_SAMPLE_COUNT);
        assert_eq!(
            wrapped.diagnostics(0, 0, 0).monitor_turnaround_p95_ms,
            114.0
        );
    }

    #[test]
    fn adaptive_preview_ignores_drag_time_proxy_turnaround() {
        assert!(adaptive_preview_can_observe(PreviewQuality::Auto, false));
        assert!(!adaptive_preview_can_observe(PreviewQuality::Auto, true));
        assert!(!adaptive_preview_can_observe(PreviewQuality::Full, false));
    }

    #[test]
    fn adaptive_preview_downshifts_under_pressure_and_recovers_with_hysteresis() {
        let mut controller = AdaptivePreviewController::default();
        controller.sync_sources(preview_sources([preview_source(0, 1)]));
        for _ in 0..AUTO_PREVIEW_SLOW_SAMPLES - 1 {
            assert_eq!(controller.observe(0, Duration::from_millis(20), 16.0), None);
        }
        assert_eq!(
            controller.observe(0, Duration::from_millis(20), 16.0),
            Some(PreviewQuality::Half)
        );
        for _ in 0..AUTO_PREVIEW_FAST_SAMPLES - 1 {
            assert_eq!(controller.observe(0, Duration::from_millis(4), 16.0), None);
        }
        assert_eq!(controller.resolved, PreviewQuality::Half);
        assert_eq!(
            controller.observe(0, Duration::from_millis(4), 16.0),
            Some(PreviewQuality::Full)
        );
    }

    #[test]
    fn adaptive_preview_sustained_stalls_never_downshift_below_eighth() {
        let mut controller = AdaptivePreviewController::default();
        controller.sync_sources(preview_sources([preview_source(0, 1)]));
        for expected in [
            PreviewQuality::Half,
            PreviewQuality::Quarter,
            PreviewQuality::Eighth,
        ] {
            for _ in 0..AUTO_PREVIEW_SLOW_SAMPLES - 1 {
                assert_eq!(controller.observe(0, Duration::from_millis(60), 16.0), None);
            }
            assert_eq!(
                controller.observe(0, Duration::from_millis(60), 16.0),
                Some(expected)
            );
        }
        assert_eq!(controller.observe(0, Duration::from_millis(60), 16.0), None);
        assert_eq!(controller.resolved, PreviewQuality::Eighth);
    }

    #[test]
    fn adaptive_preview_middle_band_resets_streaks_instead_of_oscillating() {
        let mut controller = AdaptivePreviewController::default();
        controller.sync_sources(preview_sources([preview_source(0, 1)]));
        for _ in 0..AUTO_PREVIEW_SLOW_SAMPLES - 1 {
            controller.observe(0, Duration::from_millis(20), 16.0);
        }
        assert_eq!(controller.observe(0, Duration::from_millis(12), 16.0), None);
        assert_eq!(controller.observe(0, Duration::from_millis(20), 16.0), None);
        assert_eq!(controller.resolved, PreviewQuality::Full);
    }

    #[test]
    fn adaptive_preview_keeps_slow_evidence_per_layer_when_three_others_are_fast() {
        let mut controller = AdaptivePreviewController::default();
        controller.sync_sources(preview_sources([
            preview_source(0, 1),
            preview_source(1, 2),
            preview_source(2, 3),
            preview_source(3, 4),
        ]));
        for sample in 0..AUTO_PREVIEW_SLOW_SAMPLES {
            assert_eq!(controller.observe(0, Duration::from_millis(4), 16.0), None);
            assert_eq!(controller.observe(1, Duration::from_millis(4), 16.0), None);
            assert_eq!(controller.observe(2, Duration::from_millis(4), 16.0), None);
            let changed = controller.observe(3, Duration::from_millis(20), 16.0);
            assert_eq!(
                changed,
                (sample + 1 == AUTO_PREVIEW_SLOW_SAMPLES).then_some(PreviewQuality::Half)
            );
        }
        assert_eq!(controller.resolved, PreviewQuality::Half);
    }

    #[test]
    fn adaptive_preview_recovers_only_after_every_active_layer_is_stably_fast() {
        let mut controller = AdaptivePreviewController::default();
        controller.sync_sources(preview_sources([
            preview_source(0, 1),
            preview_source(1, 2),
            preview_source(2, 3),
            preview_source(3, 4),
        ]));
        for _ in 0..AUTO_PREVIEW_SLOW_SAMPLES {
            controller.observe(3, Duration::from_millis(20), 16.0);
        }
        assert_eq!(controller.resolved, PreviewQuality::Half);

        for _ in 0..AUTO_PREVIEW_FAST_SAMPLES {
            assert_eq!(controller.observe(0, Duration::from_millis(4), 16.0), None);
            assert_eq!(controller.observe(1, Duration::from_millis(4), 16.0), None);
            assert_eq!(controller.observe(2, Duration::from_millis(4), 16.0), None);
        }
        for _ in 0..AUTO_PREVIEW_FAST_SAMPLES - 1 {
            assert_eq!(controller.observe(3, Duration::from_millis(4), 16.0), None);
        }
        assert_eq!(controller.resolved, PreviewQuality::Half);
        assert_eq!(
            controller.observe(3, Duration::from_millis(4), 16.0),
            Some(PreviewQuality::Full)
        );
    }

    #[test]
    fn adaptive_preview_same_media_clip_replacement_cannot_inherit_a_slow_streak() {
        let mut controller = AdaptivePreviewController::default();
        let first_clip = preview_source(0, 1);
        controller.sync_sources(preview_sources([first_clip]));
        for _ in 0..AUTO_PREVIEW_SLOW_SAMPLES - 1 {
            controller.observe(0, Duration::from_millis(20), 16.0);
        }
        controller.sync_sources(preview_sources([PreviewSourceRequest {
            clip_id: nle_timeline::ClipId(99),
            ..first_clip
        }]));
        assert_eq!(controller.observe(0, Duration::from_millis(20), 16.0), None);
        assert_eq!(controller.resolved, PreviewQuality::Full);
    }

    #[test]
    fn playback_soak_duration_parser_defaults_and_bounds_explicit_values() {
        assert_eq!(
            playback_soak_duration_seconds(None),
            DEFAULT_PLAYBACK_SOAK_SECONDS
        );
        assert_eq!(playback_soak_duration_seconds(Some("15")), 15);
        assert_eq!(playback_soak_duration_seconds(Some("0")), 1);
        assert_eq!(
            playback_soak_duration_seconds(Some("999999")),
            MAX_PLAYBACK_SOAK_SECONDS
        );
        assert_eq!(
            playback_soak_duration_seconds(Some("invalid")),
            DEFAULT_PLAYBACK_SOAK_SECONDS
        );
    }

    #[test]
    fn phase1_sustained_duration_parser_defaults_and_bounds_explicit_values() {
        assert_eq!(
            phase1_sustained_duration_seconds(None),
            DEFAULT_PHASE1_SUSTAINED_SOAK_SECONDS
        );
        assert_eq!(
            phase1_sustained_duration_seconds(Some("15")),
            MIN_PHASE1_SUSTAINED_SOAK_SECONDS
        );
        assert_eq!(
            phase1_sustained_duration_seconds(Some("0")),
            MIN_PHASE1_SUSTAINED_SOAK_SECONDS
        );
        assert_eq!(
            phase1_sustained_duration_seconds(Some("999999")),
            MAX_PHASE1_SUSTAINED_SOAK_SECONDS
        );
        assert_eq!(
            phase1_sustained_duration_seconds(Some("invalid")),
            DEFAULT_PHASE1_SUSTAINED_SOAK_SECONDS
        );
    }

    #[test]
    fn phase1_live_audio_duration_parser_defaults_and_bounds_explicit_values() {
        assert_eq!(
            phase1_live_audio_duration_seconds(None),
            DEFAULT_PHASE1_LIVE_AUDIO_SECONDS
        );
        assert_eq!(
            phase1_live_audio_duration_seconds(Some("1")),
            MIN_PHASE1_LIVE_AUDIO_SECONDS
        );
        assert_eq!(
            phase1_live_audio_duration_seconds(Some("999")),
            MAX_PHASE1_LIVE_AUDIO_SECONDS
        );
        assert_eq!(
            phase1_live_audio_duration_seconds(Some("invalid")),
            DEFAULT_PHASE1_LIVE_AUDIO_SECONDS
        );
    }

    #[test]
    fn phase1_sustained_dropped_frame_limit_keeps_startup_and_rate_bounds() {
        assert_eq!(phase1_sustained_dropped_frame_limit(0), 4);
        assert_eq!(phase1_sustained_dropped_frame_limit(4_000), 4);
        assert_eq!(phase1_sustained_dropped_frame_limit(4_001), 5);
        assert_eq!(phase1_sustained_dropped_frame_limit(1_488_000), 1_488);
    }

    #[test]
    fn playback_soak_starts_after_transport_and_reports_counter_deltas_and_loops() {
        let (report_tx, report_rx) = mpsc::sync_channel(1);
        let mut probe = PlaybackSoakProbe {
            requested_duration: Duration::from_secs(10),
            started_at: None,
            baseline_diagnostics: None,
            loop_count: 0,
            audio_fault_observed: false,
            unexpected_playback_stop_observed: false,
            report_tx: Some(report_tx),
        };
        let started_at = Instant::now();
        let baseline = RuntimeDiagnostics {
            monitor_requests: 10,
            monitor_completed_frames: 8,
            monitor_presented_frames: 7,
            native_viewer_uploads: 7,
            ..Default::default()
        };
        assert!(!probe.is_started());
        probe.start_after_real_playback(started_at, baseline);
        assert!(probe.is_started());
        assert!(!probe.due(started_at + Duration::from_secs(9)));
        probe.record_loop();
        probe.record_loop();
        let report = probe
            .report(
                started_at + Duration::from_secs(10),
                RuntimeDiagnostics {
                    monitor_requests: 16,
                    monitor_completed_frames: 13,
                    monitor_presented_frames: 12,
                    monitor_dropped_frames: 2,
                    monitor_hold_events: 1,
                    monitor_late_frames: 1,
                    monitor_turnaround_p95_ms: 31.5,
                    native_viewer_uploads: 12,
                    audio_underrun_frames: 48,
                    ..Default::default()
                },
                vec!["Software".to_owned()],
                "Full".to_owned(),
                "Full".to_owned(),
                512 * 1024 * 1024,
                PlaybackSoakMonitorResources {
                    frame_cache_capacity_bytes: 512 * 1024 * 1024,
                    current_frame_cache_bytes: 128 * 1024 * 1024,
                    peak_frame_cache_bytes_upper_bound: 256 * 1024 * 1024,
                    active_sticky_sessions: 1,
                    peak_sticky_sessions: 2,
                    session_cap: 4,
                    active_foreground_sessions: 1,
                    foreground_session_cap: 2,
                    active_background_sessions: 0,
                    background_session_cap: 2,
                    live_source_groups: 1,
                    source_group_cap: 4,
                    live_lane_actors: 1,
                    lane_actor_cap: 8,
                    retiring_lane_actors: 0,
                },
                DecoderStageTimingsReport::default(),
                true,
            )
            .expect("transport start makes a soak report ready");
        assert_eq!(report.schema_version, 5);
        assert_eq!(report.actual_duration_seconds, 10.0);
        assert_eq!(report.loop_count, 2);
        assert_eq!(report.runtime_diagnostics_delta.monitor_requests, 6);
        assert_eq!(report.runtime_diagnostics_delta.native_viewer_uploads, 5);
        assert_eq!(report.runtime_diagnostics_delta.audio_underrun_frames, 48);
        assert_eq!(
            report
                .runtime_diagnostics_delta
                .monitor_turnaround_window_p95_ms,
            31.5
        );
        assert!(probe.publish(report));
        let published = report_rx.try_recv().expect("one-shot soak report");
        assert_eq!(published.observed_decoder_backends, ["Software"]);
        assert_eq!(published.monitor_cache_cap_bytes, 512 * 1024 * 1024);
        assert_eq!(
            published.monitor_resources.current_frame_cache_bytes,
            128 * 1024 * 1024
        );
        assert_eq!(published.monitor_resources.peak_sticky_sessions, 2);
        let json = serde_json::to_value(&published).expect("soak report serializes");
        assert_eq!(
            json.pointer("/monitor_resources/peak_frame_cache_bytes_upper_bound"),
            Some(&serde_json::Value::from(256 * 1024 * 1024))
        );
        assert!(published.audio_transport_healthy_at_completion);
        assert!(!published.audio_fault_observed);
        assert!(!published.unexpected_playback_stop_observed);
        assert!(!probe.publish(published));
    }

    #[test]
    fn playback_soak_rejects_audio_faults_and_stops_before_timeline_end() {
        let (report_tx, _report_rx) = mpsc::sync_channel(1);
        let mut probe = PlaybackSoakProbe {
            requested_duration: Duration::from_secs(1),
            started_at: None,
            baseline_diagnostics: None,
            loop_count: 0,
            audio_fault_observed: false,
            unexpected_playback_stop_observed: false,
            report_tx: Some(report_tx),
        };
        let started_at = Instant::now();
        probe.start_after_real_playback(started_at, RuntimeDiagnostics::default());
        probe.observe_transport_state(true, false, false, false);
        let report = probe
            .report(
                started_at + Duration::from_secs(1),
                RuntimeDiagnostics::default(),
                vec!["Software".to_owned()],
                "Full".to_owned(),
                "Full".to_owned(),
                512 * 1024 * 1024,
                PlaybackSoakMonitorResources {
                    frame_cache_capacity_bytes: 512 * 1024 * 1024,
                    current_frame_cache_bytes: 0,
                    peak_frame_cache_bytes_upper_bound: 0,
                    active_sticky_sessions: 0,
                    peak_sticky_sessions: 0,
                    session_cap: 4,
                    active_foreground_sessions: 0,
                    foreground_session_cap: 2,
                    active_background_sessions: 0,
                    background_session_cap: 2,
                    live_source_groups: 0,
                    source_group_cap: 4,
                    live_lane_actors: 0,
                    lane_actor_cap: 8,
                    retiring_lane_actors: 0,
                },
                DecoderStageTimingsReport::default(),
                false,
            )
            .expect("started probe reports its failure evidence");
        assert!(!report.audio_transport_healthy_at_completion);
        assert!(report.audio_fault_observed);
        assert!(report.unexpected_playback_stop_observed);
        assert_eq!(report.loop_count, 0);
    }

    #[test]
    fn playback_soak_monitor_resources_uses_exact_shared_session_pool_diagnostics() {
        let resources = aggregate_playback_soak_monitor_resource_diagnostics(
            nle_decode::MonitorFrameCachePoolDiagnostics {
                capacity_bytes: 512,
                current_bytes: 120,
                peak_bytes: 225,
                eviction_count: 0,
            },
            nle_decode::MonitorSessionPoolDiagnostics {
                active_sticky_sessions: 3,
                peak_sticky_sessions: 5,
                session_cap: 8,
                active_foreground_sessions: 2,
                foreground_session_cap: 4,
                active_background_sessions: 1,
                background_session_cap: 4,
            },
            nle_decode::MonitorSourceCoordinatorDiagnostics {
                live_source_groups: 2,
                source_group_cap: 4,
                live_lane_actors: 3,
                lane_actor_cap: 8,
                retiring_lane_actors: 1,
            },
        );

        assert_eq!(
            resources,
            PlaybackSoakMonitorResources {
                frame_cache_capacity_bytes: 512,
                current_frame_cache_bytes: 120,
                peak_frame_cache_bytes_upper_bound: 225,
                active_sticky_sessions: 3,
                peak_sticky_sessions: 5,
                session_cap: 8,
                active_foreground_sessions: 2,
                foreground_session_cap: 4,
                active_background_sessions: 1,
                background_session_cap: 4,
                live_source_groups: 2,
                source_group_cap: 4,
                live_lane_actors: 3,
                lane_actor_cap: 8,
                retiring_lane_actors: 1,
            }
        );
    }

    #[test]
    fn surface_submission_probe_reports_a_bounded_rolling_surface_window() {
        let (report_tx, report_rx) = mpsc::sync_channel(1);
        let mut probe = SurfaceSubmissionProbe {
            cpu_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            intervals_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            present_call_ms: [0.0; FRAME_TIME_SAMPLE_COUNT],
            sample_count: 0,
            last_submitted_at: None,
            completed: None,
            report_tx: Some(report_tx),
        };
        let origin = Instant::now();
        assert!(probe.record(Duration::from_millis(2), Duration::from_millis(1), origin));
        for frame in 1..=FRAME_TIME_SAMPLE_COUNT {
            let keep_running = probe.record(
                Duration::from_millis(2),
                Duration::from_millis(1),
                origin + Duration::from_millis(frame as u64 * 16),
            );
            assert_eq!(keep_running, frame < FRAME_TIME_SAMPLE_COUNT);
        }
        assert!(report_rx.try_recv().is_err());
        let runtime_diagnostics = RuntimeDiagnosticsReport {
            monitor_requests: 11,
            monitor_completed_frames: 12,
            monitor_presented_frames: 13,
            monitor_dropped_frames: 14,
            monitor_hold_events: 15,
            monitor_late_frames: 16,
            monitor_errors: 17,
            monitor_turnaround_window_p95_ms: 18.5,
            native_viewer_uploads: 18,
            fallback_viewer_uploads: 19,
            audio_underrun_frames: 20,
            audio_callback_lock_failures: 21,
            audio_late_discarded_frames: 22,
        };
        assert!(
            probe.publish(SurfaceReportEnvironment {
                renderer: RendererReport {
                    name: "Test GPU".to_owned(),
                    vendor_id: 1,
                    device_id: 2,
                    device_type: "DiscreteGpu".to_owned(),
                    backend: "Test".to_owned(),
                    driver: "driver".to_owned(),
                    driver_info: "1.0".to_owned(),
                },
                decoder_backends: vec!["Software".to_owned()],
                encoder_backend: "libopenh264".to_owned(),
                machine: MachineProfile {
                    cpu_identity: Some("Test CPU".to_owned()),
                    logical_cpu_count: 8,
                    total_physical_memory_bytes: Some(16 * 1024 * 1024 * 1024),
                },
                selected_preview_quality: "Auto".to_owned(),
                resolved_preview_quality: "Half".to_owned(),
                preview_size: [960, 540],
                monitor_cache_cap_bytes: 512 * 1024 * 1024,
                display_refresh_millihertz: Some(60_000),
                decoder_stage_timings: nle_decode::MonitorDecoderStageTimings {
                    worker_request: nle_decode::MonitorStageTiming {
                        samples: 2,
                        total_nanos: 5_000_000,
                        max_nanos: 4_000_000,
                    },
                    ..Default::default()
                }
                .into(),
                viewer_stage_timings: ViewerStageTimingsReport {
                    upload_cpu: ViewerStageTimingReport {
                        samples: 2,
                        p95_ms: 1.0,
                        max_ms: 2.0,
                    },
                    compositor_encode_cpu: ViewerStageTimingReport {
                        samples: 1,
                        p95_ms: 3.0,
                        max_ms: 3.0,
                    },
                },
                gpu_stage_timings: GpuStageTimingsReport {
                    timestamp_query_supported: true,
                    composite_pass_gpu: Some(ViewerStageTimingReport {
                        samples: 1,
                        p95_ms: 1.5,
                        max_ms: 1.5,
                    }),
                    submission_to_completion_elapsed: ViewerStageTimingReport {
                        samples: 2,
                        p95_ms: 4.0,
                        max_ms: 5.0,
                    },
                },
                audio_stage_timings: AudioStageTimingsReport {
                    output_callback_cpu: nle_audio::AudioCallbackCpuTiming {
                        samples: 2,
                        total_nanos: 5_000_000,
                        max_nanos: 4_000_000,
                    }
                    .into(),
                    mix_render_cpu: nle_audio::AudioCallbackCpuTiming {
                        samples: 2,
                        total_nanos: 3_000_000,
                        max_nanos: 2_000_000,
                    }
                    .into(),
                },
                runtime_diagnostics,
            })
        );
        let report = report_rx.try_recv().expect("surface submission report");
        assert_eq!(report.schema_version, 7);
        assert_eq!(report.samples, FRAME_TIME_SAMPLE_COUNT);
        assert_eq!(report.cpu_p95_ms, 2.0);
        assert_eq!(report.surface_submission_interval_p95_ms, 16.0);
        assert_eq!(report.surface_present_call_cpu_p95_ms, 1.0);
        assert!((report.average_submission_fps - 62.5).abs() < 0.01);
        assert_eq!(report.decoder_backends, ["Software"]);
        assert_eq!(report.encoder_backend, "libopenh264");
        assert_eq!(report.renderer_device_type, "DiscreteGpu");
        assert_eq!(report.resolved_preview_quality, "Half");
        assert_eq!(report.monitor_cache_cap_bytes, 512 * 1024 * 1024);
        assert_eq!(report.decoder_stage_timings.worker_request.samples, 2);
        assert!((report.decoder_stage_timings.worker_request.total_ms - 5.0).abs() < f64::EPSILON);
        assert!((report.decoder_stage_timings.worker_request.mean_ms - 2.5).abs() < f64::EPSILON);
        assert_eq!(report.viewer_stage_timings.upload_cpu.samples, 2);
        assert_eq!(
            report.viewer_stage_timings.compositor_encode_cpu.max_ms,
            3.0
        );
        assert_eq!(
            report
                .gpu_stage_timings
                .composite_pass_gpu
                .expect("timestamp-supported report includes composite pass")
                .p95_ms,
            1.5
        );
        assert!(report.gpu_stage_timings.timestamp_query_supported);
        assert_eq!(
            report
                .gpu_stage_timings
                .submission_to_completion_elapsed
                .samples,
            2
        );
        assert_eq!(
            report
                .gpu_stage_timings
                .submission_to_completion_elapsed
                .max_ms,
            5.0
        );
        assert_eq!(report.audio_stage_timings.output_callback_cpu.samples, 2);
        assert_eq!(report.audio_stage_timings.mix_render_cpu.samples, 2);
        assert_eq!(report.runtime_diagnostics, runtime_diagnostics);
        assert!(
            (report.audio_stage_timings.output_callback_cpu.total_ms - 5.0).abs() < f64::EPSILON
        );
        assert!(
            (report.audio_stage_timings.output_callback_cpu.mean_ms - 2.5).abs() < f64::EPSILON
        );
        assert!((report.audio_stage_timings.mix_render_cpu.total_ms - 3.0).abs() < f64::EPSILON);
        assert!((report.audio_stage_timings.mix_render_cpu.mean_ms - 1.5).abs() < f64::EPSILON);
        let json = serde_json::to_value(&report).expect("surface report serializes");
        assert_eq!(
            json.pointer("/viewer_stage_timings/upload_cpu/samples"),
            Some(&serde_json::Value::from(2))
        );
        assert_eq!(
            json.pointer("/gpu_stage_timings/composite_pass_gpu/p95_ms"),
            Some(&serde_json::Value::from(1.5))
        );
        assert_eq!(
            json.pointer("/gpu_stage_timings/submission_to_completion_elapsed/p95_ms"),
            Some(&serde_json::Value::from(4.0))
        );
        assert_eq!(
            json.pointer("/surface_present_call_cpu_p95_ms"),
            Some(&serde_json::Value::from(1.0))
        );
        assert_eq!(
            json.pointer("/renderer_device_type"),
            Some(&serde_json::Value::from("DiscreteGpu"))
        );
        assert_eq!(
            json.pointer("/audio_stage_timings/output_callback_cpu/max_ms"),
            Some(&serde_json::Value::from(4.0))
        );
        assert_eq!(
            json.pointer("/audio_stage_timings/mix_render_cpu/max_ms"),
            Some(&serde_json::Value::from(2.0))
        );
        assert_eq!(
            json.pointer("/runtime_diagnostics/monitor_dropped_frames"),
            Some(&serde_json::Value::from(14))
        );
        assert_eq!(
            json.pointer("/runtime_diagnostics/audio_underrun_frames"),
            Some(&serde_json::Value::from(20))
        );
    }

    #[test]
    fn full_surface_report_waits_for_observed_media_backends() {
        let decoder = vec!["Windows D3D11VA".to_owned()];
        assert!(surface_report_backends_ready(false, &[], None));
        assert!(!surface_report_backends_ready(true, &[], None));
        assert!(!surface_report_backends_ready(true, &decoder, None));
        assert!(surface_report_backends_ready(
            true,
            &decoder,
            Some("h264_qsv")
        ));

        let mut timings = DecoderStageTimingsReport::default();
        assert!(surface_report_stage_timings_ready(false, &timings));
        assert!(!surface_report_stage_timings_ready(true, &timings));
        let observed = DecoderStageTimingReport {
            samples: 1,
            ..Default::default()
        };
        timings.cache_lookup = observed;
        timings.demux_packet = observed;
        timings.decoder_calls = observed;
        timings.scaler = observed;
        timings.rgba_copy_letterbox = observed;
        timings.worker_request = observed;
        assert!(surface_report_stage_timings_ready(true, &timings));

        let empty = ViewerStageTimingsReport::default();
        assert!(surface_report_viewer_stage_timings_ready(false, empty));
        assert!(!surface_report_viewer_stage_timings_ready(true, empty));
        assert!(surface_report_viewer_stage_timings_ready(
            true,
            ViewerStageTimingsReport {
                upload_cpu: ViewerStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
                compositor_encode_cpu: ViewerStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
            }
        ));

        let empty = GpuStageTimingsReport::default();
        assert!(surface_report_gpu_stage_timings_ready(false, empty));
        assert!(!surface_report_gpu_stage_timings_ready(true, empty));
        assert!(surface_report_gpu_stage_timings_ready(
            true,
            GpuStageTimingsReport {
                submission_to_completion_elapsed: ViewerStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
                ..Default::default()
            }
        ));
        assert!(!surface_report_gpu_stage_timings_ready(
            true,
            GpuStageTimingsReport {
                timestamp_query_supported: true,
                composite_pass_gpu: Some(ViewerStageTimingReport::default()),
                submission_to_completion_elapsed: ViewerStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
            }
        ));
        assert!(surface_report_gpu_stage_timings_ready(
            true,
            GpuStageTimingsReport {
                timestamp_query_supported: true,
                composite_pass_gpu: Some(ViewerStageTimingReport {
                    samples: 1,
                    ..Default::default()
                }),
                submission_to_completion_elapsed: ViewerStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
            }
        ));
        let unsupported = GpuStageTimingsReport::from_snapshots(
            nle_render::ViewerCompositorGpuTiming::default(),
            nle_render::GpuSubmissionCompletionTiming {
                samples: 1,
                p95_ms: 2.0,
                max_ms: 2.0,
            },
        );
        assert!(!unsupported.timestamp_query_supported);
        assert!(unsupported.composite_pass_gpu.is_none());
        assert!(surface_report_gpu_stage_timings_ready(true, unsupported));
        let unsupported_json = serde_json::to_value(unsupported).expect("GPU timings serialize");
        assert_eq!(
            unsupported_json.pointer("/composite_pass_gpu"),
            Some(&serde_json::Value::Null)
        );

        let empty = AudioStageTimingsReport::default();
        assert!(surface_report_audio_stage_timings_ready(false, empty));
        assert!(!surface_report_audio_stage_timings_ready(true, empty));
        assert!(!surface_report_audio_stage_timings_ready(
            true,
            AudioStageTimingsReport {
                output_callback_cpu: AudioStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
                ..Default::default()
            }
        ));
        assert!(!surface_report_audio_stage_timings_ready(
            true,
            AudioStageTimingsReport {
                mix_render_cpu: AudioStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
                ..Default::default()
            }
        ));
        assert!(surface_report_audio_stage_timings_ready(
            true,
            AudioStageTimingsReport {
                output_callback_cpu: AudioStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
                mix_render_cpu: AudioStageTimingReport {
                    samples: 1,
                    ..Default::default()
                },
            }
        ));
    }

    #[test]
    fn viewer_stage_timing_window_wraps_and_reports_p95_and_max() {
        let mut window = ViewerStageTimingWindow::default();
        for milliseconds in 1..=FRAME_TIME_SAMPLE_COUNT as u64 + 1 {
            window.record(Duration::from_millis(milliseconds));
        }
        let timing = window.snapshot();
        assert_eq!(timing.samples, FRAME_TIME_SAMPLE_COUNT);
        assert_eq!(timing.p95_ms, 115.0);
        assert_eq!(timing.max_ms, 121.0);
    }

    #[test]
    fn live_pipeline_stage_samples_keep_mean_and_p95_semantics_distinct() {
        let decoder = live_mean_stage_sample(
            nle_decode::MonitorStageTiming {
                samples: 2,
                total_nanos: 5_000_000,
                max_nanos: 3_000_000,
            }
            .into(),
        )
        .expect("decoder stage sample");
        assert_eq!(
            decoder.representative,
            LivePipelineTimingRepresentative::Mean
        );
        assert_eq!(decoder.representative_ms, 2.5);
        assert_eq!(decoder.max_ms, 3.0);
        assert_eq!(decoder.samples, 2);

        let viewer = live_p95_stage_sample(24, 1.25, 2.5).expect("viewer stage sample");
        assert_eq!(viewer.representative, LivePipelineTimingRepresentative::P95);
        assert_eq!(viewer.representative_ms, 1.25);
        assert_eq!(viewer.max_ms, 2.5);
        assert_eq!(viewer.samples, 24);

        let audio = live_audio_mean_stage_sample(nle_audio::AudioCallbackCpuTiming {
            samples: 4,
            total_nanos: 2_000_000,
            max_nanos: 750_000,
        })
        .expect("audio stage sample");
        assert_eq!(audio.representative, LivePipelineTimingRepresentative::Mean);
        assert_eq!(audio.representative_ms, 0.5);
        assert_eq!(audio.max_ms, 0.75);
        assert_eq!(audio.samples, 4);

        assert!(live_p95_stage_sample(0, 0.0, 0.0).is_none());
        assert!(
            live_audio_mean_stage_sample(nle_audio::AudioCallbackCpuTiming::default()).is_none()
        );
    }

    #[test]
    fn app_live_pipeline_snapshot_reports_context_without_renderer_or_audio_device() {
        let mut app = App::new_without_startup_or_audio_for_monitor_contract();
        assert!(app.editor.set_preview_quality(PreviewQuality::Auto));
        assert!(app.editor.set_auto_preview_quality(PreviewQuality::Half));
        app.surface_present_timings
            .record(Duration::from_micros(750));
        let timing = app.live_pipeline_timing(nle_audio::AudioRuntimeDiagnostics {
            mix_render_cpu_timing: nle_audio::AudioCallbackCpuTiming {
                samples: 2,
                total_nanos: 1_000_000,
                max_nanos: 750_000,
            },
            ..Default::default()
        });

        assert_eq!(timing.active_video_layers, 0);
        assert_eq!(timing.selected_preview_quality, PreviewQuality::Auto);
        assert_eq!(timing.resolved_preview_quality, PreviewQuality::Half);
        assert_eq!(
            timing
                .sample(LivePipelineTimingStage::AudioMix)
                .expect("audio mix stage")
                .representative,
            LivePipelineTimingRepresentative::Mean
        );
        assert_eq!(
            timing
                .sample(LivePipelineTimingStage::SurfacePresentCall)
                .expect("surface present stage")
                .representative,
            LivePipelineTimingRepresentative::P95
        );
        assert!(
            timing
                .sample(LivePipelineTimingStage::CompositorCpuEncode)
                .is_none()
        );
        assert!(
            timing
                .sample(LivePipelineTimingStage::CompositorGpu)
                .is_none()
        );

        app.editor.add_media_paths([
            PathBuf::from("hud-layer-1.mp4"),
            PathBuf::from("hud-layer-2.mp4"),
            PathBuf::from("hud-layer-3.mp4"),
            PathBuf::from("hud-layer-4.mp4"),
        ]);
        let mut video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while video_tracks.len() < MONITOR_LAYER_COUNT {
            video_tracks.push(
                app.editor
                    .timeline
                    .add_track(nle_timeline::TrackKind::Video),
            );
        }
        for (index, track_id) in video_tracks
            .into_iter()
            .take(MONITOR_LAYER_COUNT)
            .enumerate()
        {
            app.editor
                .timeline
                .insert_clip(
                    track_id,
                    nle_timeline::MediaId(u32::try_from(index + 1).expect("media ID")),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(1_000_000),
                    nle_timeline::Tick(0),
                )
                .expect("insert HUD layer");
        }
        app.editor.set_playhead(nle_timeline::Tick(500_000));
        assert_eq!(
            app.live_pipeline_timing(nle_audio::AudioRuntimeDiagnostics::default())
                .active_video_layers,
            MONITOR_LAYER_COUNT
        );
    }

    #[test]
    fn encoder_fallback_report_tracks_the_latest_started_backend() {
        let mut observed = None;
        observe_encoder_backend(&mut observed, nle_export::H264Encoder::Nvidia);
        assert_eq!(observed.as_deref(), Some("h264_nvenc"));
        observe_encoder_backend(&mut observed, nle_export::H264Encoder::OpenH264);
        assert_eq!(observed.as_deref(), Some("libopenh264"));
    }

    #[test]
    fn media_acceptance_probe_reports_only_after_every_real_media_signal() {
        let (report_tx, report_rx) = mpsc::sync_channel(1);
        let mut probe = MediaAcceptanceProbe {
            media_id: 7,
            media_pool_drag_completed: true,
            viewer_panel_height: 580.0,
            timeline_panel_height: 420.0,
            timeline_view_span_ticks: 16_000_000,
            timeline_end_ticks: 15_000_000,
            linked_video_bars: 1,
            linked_audio_bars: 1,
            analysis_metadata_ready: false,
            waveform_ready: false,
            waveform_peak_count: 0,
            monitor_frame_arrived: false,
            native_viewer_uploaded: false,
            playback_start_tick: 0,
            playhead_advanced_ticks: 0,
            live_audio_meter_nonzero: false,
            pre_gain_meter_peak: 0.0,
            fade_reduction_requested_at_tick: None,
            live_fade_reduced: false,
            fade_clear_requested_at_tick: None,
            live_fade_recovered: false,
            gain_reduction_requested_at_tick: None,
            live_gain_reduced: false,
            export_started: false,
            export_progress_received: false,
            playhead_advanced_while_exporting: false,
            export_cancel_requested: false,
            export_cancelled: false,
            report_tx: Some(report_tx),
        };
        probe.record_export_started(true);
        probe.record_analysis(true, 48);
        probe.record_resolved_timeline(63_000_000, 60_000_000);
        probe.record_monitor_frame(7, true);
        probe.record_playback(249_999, 0.2, 0.1, true);
        assert!(!probe.should_cancel_export());
        probe.record_export_progress();
        assert!(!probe.should_cancel_export());
        probe.record_playback(250_000, 0.2, 0.1, true);
        assert!(probe.should_request_fade_reduction());
        probe.record_fade_reduction_requested(250_000);
        probe.record_playback(350_000, 0.005, 0.004, true);
        assert!(probe.live_fade_reduced);
        assert!(probe.should_clear_fade());
        probe.record_fade_clear_requested(350_000);
        probe.record_playback(450_000, 0.15, 0.1, true);
        assert!(probe.live_fade_recovered);
        assert!(probe.should_request_gain_reduction());
        probe.record_gain_reduction_requested(450_000);
        probe.record_playback(550_000, 0.000_5, 0.000_4, true);
        assert!(probe.should_cancel_export());
        probe.record_export_cancel_requested();
        probe.record_export_cancelled();
        let report = report_rx.try_recv().expect("media acceptance report");
        assert!(report.media_pool_drag_completed);
        assert_eq!(report.viewer_panel_height, 580.0);
        assert_eq!(report.timeline_panel_height, 420.0);
        assert_eq!(report.timeline_view_span_ticks, 63_000_000);
        assert_eq!(report.timeline_end_ticks, 60_000_000);
        assert_eq!((report.linked_video_bars, report.linked_audio_bars), (1, 1));
        assert!(report.analysis_metadata_ready && report.waveform_ready);
        assert_eq!(report.waveform_peak_count, 48);
        assert!(report.monitor_frame_arrived && report.native_viewer_uploaded);
        assert!(report.live_audio_meter_nonzero);
        assert!(report.live_fade_reduced && report.live_fade_recovered);
        assert!(report.live_gain_reduced);
        assert_eq!(report.playhead_advanced_ticks, 550_000);
        assert!(report.export_started);
        assert!(report.export_progress_received);
        assert!(report.playhead_advanced_while_exporting);
        assert!(report.export_cancelled);
    }

    #[test]
    fn media_acceptance_probe_rejects_missing_linked_audio_or_empty_waveform() {
        let (report_tx, report_rx) = mpsc::sync_channel(1);
        let mut probe = MediaAcceptanceProbe {
            media_id: 7,
            media_pool_drag_completed: true,
            viewer_panel_height: 580.0,
            timeline_panel_height: 420.0,
            timeline_view_span_ticks: 16_000_000,
            timeline_end_ticks: 15_000_000,
            linked_video_bars: 1,
            linked_audio_bars: 0,
            analysis_metadata_ready: false,
            waveform_ready: false,
            waveform_peak_count: 0,
            monitor_frame_arrived: false,
            native_viewer_uploaded: false,
            playback_start_tick: 0,
            playhead_advanced_ticks: 0,
            live_audio_meter_nonzero: false,
            pre_gain_meter_peak: 0.0,
            fade_reduction_requested_at_tick: None,
            live_fade_reduced: false,
            fade_clear_requested_at_tick: None,
            live_fade_recovered: false,
            gain_reduction_requested_at_tick: None,
            live_gain_reduced: false,
            export_started: false,
            export_progress_received: false,
            playhead_advanced_while_exporting: false,
            export_cancel_requested: false,
            export_cancelled: false,
            report_tx: Some(report_tx),
        };
        probe.record_analysis(true, 0);
        probe.record_monitor_frame(7, false);
        probe.record_playback(1_000_000, 0.5, 0.5, false);
        assert!(!probe.ready());
        assert!(report_rx.try_recv().is_err());
    }

    #[test]
    fn packaged_layout_rejects_the_old_oversized_timeline_but_allows_small_window_adaptation() {
        assert!(editor_layout_is_balanced(580.0, 420.0));
        assert!(editor_layout_is_balanced(270.0, 336.0));
        assert!(!editor_layout_is_balanced(1_000.0, 1.0));
        assert!(!editor_layout_is_balanced(300.0, 580.0));
        assert!(!editor_layout_is_balanced(f32::NAN, 420.0));
        assert!(timeline_view_fits_content(16_000_000, 15_000_000));
        assert!(!timeline_view_fits_content(252_000_000, 15_000_000));
        assert!(!timeline_view_fits_content(10_000_000, 15_000_000));
    }

    #[test]
    fn audio_device_clock_maps_to_the_same_timeline_tick_for_video() {
        assert_eq!(
            audio_master_timeline_tick(5_000_000, 12_000_000, 12_500_000),
            5_500_000
        );
        assert_eq!(
            audio_master_timeline_tick(5_000_000, 12_000_000, 11_750_000),
            4_750_000,
            "backward seeks preserve the shared A/V clock mapping"
        );
    }

    #[test]
    fn crossfade_lane_boundaries_reconcile_only_when_retained_audio_is_continuous() {
        let key = |clip_id| AudioClipKey {
            track_id: nle_timeline::TrackId(1),
            clip_id: nle_timeline::ClipId(clip_id),
            path: PathBuf::from(format!("clip-{clip_id}.wav")),
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 4_000_000,
            transition: None,
        };
        let outgoing = key(10);
        let incoming = key(11);
        let current = AudioTransportState {
            keys: vec![outgoing.clone()],
            source_ticks: vec![1_000_000],
            source_tick: 1_000_000,
            timeline_tick: 0,
            started_at: Instant::now(),
        };
        assert!(retained_audio_lanes_are_continuous(
            &current,
            &[outgoing.clone(), incoming.clone()],
            &[1_500_000, 500_000],
            500_000,
            40_000,
        ));
        assert!(!retained_audio_lanes_are_continuous(
            &current,
            &[outgoing.clone(), incoming.clone()],
            &[2_500_000, 500_000],
            500_000,
            40_000,
        ));

        let overlap = AudioTransportState {
            keys: vec![outgoing, incoming.clone()],
            source_ticks: vec![1_500_000, 500_000],
            source_tick: 1_500_000,
            timeline_tick: 500_000,
            started_at: Instant::now(),
        };
        assert!(retained_audio_lanes_are_continuous(
            &overlap,
            &[incoming],
            &[1_000_000],
            500_000,
            40_000,
        ));
    }

    #[test]
    fn native_audio_effect_mapping_preserves_order_and_skips_bypassed_or_unsupported_nodes() {
        let mapped = native_audio_effects(&[
            nle_timeline::AudioEffect::HighPass { hz: 96_000 },
            nle_timeline::AudioEffect::Bypassed(Box::new(nle_timeline::AudioEffect::LowPass {
                hz: 800,
            })),
            nle_timeline::AudioEffect::Compressor,
            nle_timeline::AudioEffect::StereoWidth { width: 1.25 },
        ]);
        assert_eq!(
            mapped,
            vec![
                nle_audio::AudioProcessorSpec::HighPass { hz: 20_000 },
                nle_audio::AudioProcessorSpec::StereoWidth { width: 1.25 },
            ]
        );
    }

    #[test]
    fn monitor_deferred_request_remains_current_until_retry_converges() {
        let mut app = App::new_with_catalog(false, None);
        let key = MonitorRequestKey {
            project_epoch: 7,
            media_id: 42,
            source_tick: 1_500_000,
            width: 1920,
            height: 1080,
            is_scrubbing: false,
            prewarm_scrub_workers: false,
            high_quality_scaling: true,
            selected_quality: PreviewQuality::Full,
            resolved_quality: PreviewQuality::Full,
            source_frame_rate: nle_ui_core::SourceFrameRate::new(30, 1),
            source_frame_duration_tick: None,
        };
        let identity = MonitorSourceIdentity {
            media_id: 42,
            path: PathBuf::from("deferred-source.mp4"),
            acceleration: nle_decode::AccelerationPreference::PreferHardware,
        };
        app.record_monitor_request_submission(1, key, identity.clone(), 99, true);

        assert_eq!(app.monitor_last_requests[1], Some(key));
        assert_eq!(app.monitor_source_identities[1], Some(identity));
        assert_eq!(app.monitor_latest_request_ids[1], 99);
        assert!(app.monitor_requests_in_flight[1]);
        assert!(app.monitor_request_deferred[1]);
        assert_eq!(
            app.monitor_request_started_at[1].map(|(id, _)| id),
            Some(99)
        );

        // Retry acceptance preserves the retained request ID; frame/error handling then owns
        // normal convergence and clears the in-flight state rather than minting a stale ID.
        app.monitor_request_deferred[1] = false;
        assert_eq!(app.monitor_latest_request_ids[1], 99);
        assert!(app.monitor_requests_in_flight[1]);
    }

    #[test]
    fn monitor_source_identity_changes_for_path_or_acceleration_only() {
        let identity = MonitorSourceIdentity {
            media_id: 7,
            path: PathBuf::from("same-media.mp4"),
            acceleration: nle_decode::AccelerationPreference::Auto,
        };
        assert!(!monitor_source_identity_changed(
            Some(&identity),
            7,
            Path::new("same-media.mp4"),
            nle_decode::AccelerationPreference::Auto,
        ));
        assert!(monitor_source_identity_changed(
            Some(&identity),
            7,
            Path::new("replacement-path.mp4"),
            nle_decode::AccelerationPreference::Auto,
        ));
        assert!(monitor_source_identity_changed(
            Some(&identity),
            7,
            Path::new("same-media.mp4"),
            nle_decode::AccelerationPreference::Software,
        ));
    }

    #[test]
    fn monitor_cache_cli_accepts_split_and_equals_forms_with_safe_bounds() {
        let mib = 1024 * 1024;
        assert_eq!(
            monitor_cache_bytes_from_args(["maelstrom".into(), "--cache-mb=256".into()]),
            MIN_MONITOR_CACHE_MB * mib
        );
        assert_eq!(
            monitor_cache_bytes_from_args(["maelstrom".into(), "--cache-mb".into(), "512".into()]),
            512 * mib
        );
        assert_eq!(
            monitor_cache_bytes_from_args(["maelstrom".into(), "--cache-mb=1".into()]),
            MIN_MONITOR_CACHE_MB * mib
        );
        assert_eq!(
            monitor_cache_bytes_from_args(["maelstrom".into(), "--cache-mb=999999".into()]),
            MAX_MONITOR_CACHE_MB * mib
        );
    }

    #[test]
    fn native_timeline_color_is_straight_linear_for_translucent_colors() {
        let color = egui::Color32::from_rgba_unmultiplied(128, 64, 32, 128);
        let converted = straight_linear_color(color);
        let premultiplied = egui::Rgba::from(color).to_array();

        for channel in 0..3 {
            assert!((converted[channel] * converted[3] - premultiplied[channel]).abs() < 0.000_1);
        }
        assert!((converted[3] - premultiplied[3]).abs() < 0.000_1);
        assert!(converted[0] > premultiplied[0]);
    }

    #[test]
    fn screen_cursor_converts_to_logical_client_points_at_high_dpi() {
        let point = logical_cursor_position(700, 500, PhysicalPosition::new(100, 50), 1.5);
        assert_eq!(point, egui::Pos2::new(400.0, 300.0));
    }

    #[test]
    fn winit_logical_cursor_converts_to_egui_points() {
        let point = egui::Pos2::new(10.0, 20.0);
        assert_eq!(
            egui_point_from_winit_logical(point, 1.0, 0.8),
            egui::Pos2::new(12.5, 25.0)
        );
        assert_eq!(egui_point_from_winit_logical(point, 1.0, 1.0), point);
        assert_eq!(
            egui_point_from_winit_logical(point, 1.5, 1.2),
            egui::Pos2::new(12.5, 25.0)
        );
        assert_eq!(
            egui_point_from_winit_logical(point, 2.0, 1.6),
            egui::Pos2::new(12.5, 25.0)
        );
    }

    #[test]
    fn media_drag_pointer_claims_once_after_a_held_primary_press() {
        let mut pointer = MediaDragPointer::default();
        let first = egui::Pos2::new(10.0, 20.0);
        let second = egui::Pos2::new(30.0, 40.0);

        assert_eq!(pointer.cursor_moved(first), None);
        assert_eq!(pointer.primary_pressed(None), Some(first));
        pointer.media_drag_claimed();
        assert_eq!(pointer.cursor_moved(second), None);
        assert_eq!(pointer.primary_released(), Some(second));
        assert_eq!(pointer.cursor_moved(first), None);
    }

    #[test]
    fn media_drag_pointer_recovers_when_press_precedes_cursor_motion() {
        let mut pointer = MediaDragPointer::default();
        let point = egui::Pos2::new(10.0, 20.0);

        assert_eq!(pointer.primary_pressed(None), None);
        assert_eq!(pointer.cursor_moved(point), Some(point));
    }

    #[test]
    fn media_drag_pointer_keeps_press_origin_until_scroll_area_releases_gesture() {
        let mut pointer = MediaDragPointer::default();
        let source = egui::Pos2::new(10.0, 20.0);
        let destination = egui::Pos2::new(300.0, 400.0);

        assert_eq!(pointer.cursor_moved(source), None);
        assert_eq!(pointer.primary_pressed(Some(source)), Some(source));
        assert_eq!(pointer.cursor_moved(destination), Some(source));
        pointer.media_drag_claimed();
        assert_eq!(pointer.primary_released(), Some(destination));
    }

    #[test]
    fn media_drag_pointer_uses_press_seed_and_resets_after_focus_loss() {
        let mut pointer = MediaDragPointer::default();
        let source = egui::Pos2::new(10.0, 20.0);
        let destination = egui::Pos2::new(300.0, 400.0);

        assert_eq!(pointer.primary_pressed(Some(source)), Some(source));
        assert_eq!(pointer.cursor_moved(destination), Some(source));
        pointer.media_drag_claimed();
        pointer.reset();

        assert_eq!(pointer.cursor_moved(source), None);
        assert_eq!(pointer.primary_released(), Some(source));
    }

    #[test]
    fn media_drag_pointer_prefers_exact_button_position_over_stale_motion() {
        let mut pointer = MediaDragPointer::default();
        let stale = egui::Pos2::new(10.0, 20.0);
        let source = egui::Pos2::new(300.0, 400.0);

        assert_eq!(pointer.cursor_moved(stale), None);
        assert_eq!(pointer.primary_pressed(Some(source)), Some(source));
        assert_eq!(
            pointer.cursor_moved(egui::Pos2::new(600.0, 700.0)),
            Some(source)
        );
    }

    #[test]
    fn timeline_texture_keys_are_unique_across_project_sessions() {
        assert_ne!(timeline_texture_id(4, 7), timeline_texture_id(5, 7));
        assert_ne!(timeline_texture_id(4, 7), timeline_texture_id(4, 8));
    }

    #[test]
    fn multi_file_timeline_drop_advances_and_preserves_each_analysis_action() {
        let mut editor = EditorState::new(Language::English, "Drop Test");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        let mut start = nle_timeline::Tick(0);
        let mut analyzed = Vec::new();

        for media_id in [1, 2] {
            assert!(editor.overwrite_media_at(media_id, start));
            start = editor
                .selected_timeline_clip
                .and_then(|clip_id| editor.timeline.clip(clip_id))
                .map(|clip| clip.end())
                .expect("inserted clip has an end");
            let EditorAction::AnalyzeMedia { media_id, .. } = editor
                .take_action()
                .expect("each placement emits its own analysis request")
            else {
                panic!("unexpected editor action")
            };
            analyzed.push(media_id);
        }

        assert_eq!(analyzed, vec![1, 2]);
        assert_eq!(start, nle_timeline::Tick(30_000_000));
    }

    fn test_catalog_path(test_name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "maelstrom-catalog-{test_name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        path.join("projects.json")
    }

    fn project(id: u32, name: &str, recent: &str) -> nle_ui_core::Project {
        nle_ui_core::Project {
            id,
            name: name.to_owned(),
            recent: recent.to_owned(),
            size: "0 B".to_owned(),
            thumbnail: None,
        }
    }

    fn test_document(path: &std::path::Path, snapshot: EditorProjectSnapshot) -> ProjectDocument {
        nle_project_io::document_for_path(path, "Test", snapshot, ProjectSettings::default())
    }

    fn preview_source(layer: usize, media_id: u32) -> PreviewSourceRequest {
        PreviewSourceRequest {
            layer,
            priority: layer.saturating_add(1) as u8,
            clip_id: nle_timeline::ClipId(media_id),
            media_id,
            source_tick: 0,
            source_frame_rate: None,
            source_frame_duration_tick: None,
        }
    }

    fn preview_sources(
        sources: impl IntoIterator<Item = PreviewSourceRequest>,
    ) -> [Option<PreviewSourceRequest>; MONITOR_LAYER_COUNT] {
        let mut slots = [None; MONITOR_LAYER_COUNT];
        for source in sources {
            slots[source.layer] = Some(source);
        }
        slots
    }

    fn reconfigure_test_monitor_source_cap(app: &mut App, source_group_cap: usize) {
        let session_pool = nle_decode::MonitorSessionPool::new(source_group_cap, 0);
        let coordinator =
            nle_decode::MonitorSourceCoordinator::new(source_group_cap, session_pool.clone());
        let frame_cache_pool = app.monitor_frame_cache_pool.clone();
        app.monitor_decoders = std::array::from_fn(|_| {
            nle_decode::MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
                || {},
                frame_cache_pool.clone(),
                coordinator.clone(),
            )
        });
        app.monitor_session_pool = session_pool;
        app.monitor_source_coordinator = coordinator;
    }

    fn priority_test_app_with_paths(paths: [PathBuf; 2]) -> App {
        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths(paths);
        let video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(video_tracks.len(), 2);
        for (track, media_id) in video_tracks.into_iter().zip([1, 2]) {
            app.editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(1_000_000),
                    nle_timeline::Tick(0),
                )
                .expect("insert priority test clip");
        }
        app.editor.set_playhead(nle_timeline::Tick(100_000));
        app
    }

    fn priority_test_app() -> App {
        priority_test_app_with_paths([
            PathBuf::from("priority-lower-does-not-exist.mp4"),
            PathBuf::from("priority-upper-does-not-exist.mp4"),
        ])
    }

    #[test]
    fn contributing_video_layers_prioritize_visible_top_layers_without_allocating() {
        let mut lower = preview_source(0, 1);
        lower.priority = 4;
        let mut upper_tie = preview_source(2, 2);
        upper_tie.priority = 9;
        let mut uppermost_tie = preview_source(3, 3);
        uppermost_tie.priority = 9;
        let sources = preview_sources([lower, upper_tie, uppermost_tie]);

        let (layers, count) = contributing_video_layers_by_priority(&sources);

        assert_eq!(count, 3);
        assert_eq!(&layers[..count], &[3, 2, 0]);
        assert!(
            sources[1].is_none(),
            "release pass precedes this admission list"
        );
    }

    #[test]
    fn eviction_selection_releases_a_complete_lower_priority_shared_source_group() {
        let mut lower = preview_source(0, 1);
        lower.priority = 2;
        let mut shared_lower = preview_source(1, 1);
        shared_lower.priority = 3;
        let mut requester = preview_source(3, 2);
        requester.priority = 9;
        let sources = preview_sources([lower, shared_lower, requester]);
        let shared_identity = MonitorSourceIdentity {
            media_id: 1,
            path: PathBuf::from("shared-lower.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let requester_identity = MonitorSourceIdentity {
            media_id: 2,
            path: PathBuf::from("requester.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let identities = [
            Some(shared_identity.clone()),
            Some(shared_identity),
            None,
            Some(requester_identity.clone()),
        ];

        let selected = lower_priority_monitor_eviction_group(
            &sources,
            &identities,
            &[true, false, false, true],
            &[4, 5, 0, 10],
            3,
            &requester_identity,
        );

        assert_eq!(selected, [true, true, false, false]);
    }

    #[test]
    fn eviction_selection_protects_a_source_needed_by_equal_priority_visual_work() {
        let mut lower = preview_source(0, 1);
        lower.priority = 2;
        let mut protected = preview_source(2, 1);
        protected.priority = 9;
        let mut requester = preview_source(3, 2);
        requester.priority = 9;
        let sources = preview_sources([lower, protected, requester]);
        let shared_identity = MonitorSourceIdentity {
            media_id: 1,
            path: PathBuf::from("protected-shared.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let requester_identity = MonitorSourceIdentity {
            media_id: 2,
            path: PathBuf::from("requester.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let identities = [
            Some(shared_identity.clone()),
            None,
            Some(shared_identity),
            Some(requester_identity.clone()),
        ];

        let selected = lower_priority_monitor_eviction_group(
            &sources,
            &identities,
            &[false, false, false, true],
            &[4, 0, 5, 10],
            3,
            &requester_identity,
        );

        assert_eq!(selected, [false; MONITOR_LAYER_COUNT]);
    }

    #[test]
    fn eviction_selection_uses_oldest_request_within_the_same_visual_priority() {
        let mut older = preview_source(0, 1);
        older.priority = 3;
        let mut newer = preview_source(1, 2);
        newer.priority = 3;
        let mut requester = preview_source(3, 3);
        requester.priority = 9;
        let sources = preview_sources([older, newer, requester]);
        let older_identity = MonitorSourceIdentity {
            media_id: 1,
            path: PathBuf::from("older.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let newer_identity = MonitorSourceIdentity {
            media_id: 2,
            path: PathBuf::from("newer.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let requester_identity = MonitorSourceIdentity {
            media_id: 3,
            path: PathBuf::from("requester.mp4"),
            acceleration: nle_decode::AccelerationPreference::Software,
        };
        let identities = [
            Some(older_identity),
            Some(newer_identity),
            None,
            Some(requester_identity.clone()),
        ];

        let selected = lower_priority_monitor_eviction_group(
            &sources,
            &identities,
            &[false, false, false, true],
            &[20, 40, 0, 50],
            3,
            &requester_identity,
        );

        assert_eq!(selected, [true, false, false, false]);
    }

    #[test]
    fn deferred_monitor_retries_keep_visual_priority_and_topmost_tie_order() {
        let (layers, count) =
            selected_monitor_layers_by_priority(&[1, 9, 9, 4], &[true; MONITOR_LAYER_COUNT]);
        assert_eq!(&layers[..count], &[2, 1, 3, 0]);
    }

    #[test]
    fn priority_admission_prefers_top_source_and_releases_absent_lower_before_admitting_upper() {
        let mut app = priority_test_app();
        reconfigure_test_monitor_source_cap(&mut app, 1);

        let mut both = preview_request(&app.editor);
        both.is_scrubbing = true;
        both.output_size = [64, 36];
        app.submit_monitor_decode_request(both);
        assert!(app.monitor_last_requests[1].is_some());
        assert!(!app.monitor_request_deferred[1]);
        assert!(app.monitor_last_requests[0].is_some());
        assert!(app.monitor_request_deferred[0]);
        assert_eq!(
            app.monitor_source_coordinator
                .diagnostics()
                .live_source_groups,
            1
        );

        let mut replacement = priority_test_app();
        reconfigure_test_monitor_source_cap(&mut replacement, 1);
        let mut lower_only = preview_request(&replacement.editor);
        lower_only.is_scrubbing = true;
        lower_only.output_size = [64, 36];
        lower_only.sources[1] = None;
        replacement.submit_monitor_decode_request(lower_only);
        assert!(replacement.monitor_last_requests[0].is_some());
        assert!(!replacement.monitor_request_deferred[0]);

        let mut upper_only = preview_request(&replacement.editor);
        upper_only.is_scrubbing = true;
        upper_only.output_size = [64, 36];
        upper_only.sources[0] = None;
        replacement.submit_monitor_decode_request(upper_only);
        assert!(replacement.monitor_last_requests[0].is_none());
        assert!(replacement.monitor_last_requests[1].is_some());
        assert!(
            !replacement.monitor_request_deferred[1],
            "releasing the absent lower layer must happen before upper admission"
        );
    }

    #[test]
    fn active_lower_source_yields_to_top_priority_without_changing_audio_targets() {
        let fixture_root = test_catalog_path("priority-active-takeover")
            .parent()
            .expect("fixture root")
            .to_path_buf();
        fs::create_dir_all(&fixture_root).expect("create priority fixture directory");
        let lower_path = fixture_root.join("lower.mp4");
        let upper_path = fixture_root.join("upper.mp4");
        for path in [&lower_path, &upper_path] {
            let generated = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=64x36:rate=24",
                    "-t",
                    "1",
                    "-an",
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "5",
                ])
                .arg(path)
                .output();
            let Ok(generated) = generated else {
                let _ = fs::remove_dir_all(&fixture_root);
                return;
            };
            assert!(
                generated.status.success(),
                "create priority fixture: {}",
                String::from_utf8_lossy(&generated.stderr)
            );
        }

        let mut app = priority_test_app_with_paths([lower_path.clone(), upper_path.clone()]);
        reconfigure_test_monitor_source_cap(&mut app, 1);
        let audio_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Audio)
            .expect("priority test app has an audio track")
            .id;
        app.editor
            .timeline
            .insert_clip(
                audio_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(1_000_000),
                nle_timeline::Tick(0),
            )
            .expect("insert independent audible clip");
        let audio_before = app
            .editor
            .audio_playback_targets()
            .into_iter()
            .map(|target| {
                (
                    target.track_id,
                    target.clip_id,
                    target.media_id,
                    target.path.to_path_buf(),
                    target.source_tick,
                    target.clip_tick,
                    target.gain_db.to_bits(),
                    target.pan.to_bits(),
                    target.transition,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(audio_before.len(), 1);

        let mut lower_only = preview_request(&app.editor);
        lower_only.is_scrubbing = true;
        lower_only.output_size = [64, 36];
        lower_only.sources[1] = None;
        app.submit_monitor_decode_request(lower_only);
        assert!(!app.monitor_request_deferred[0]);
        let lower_deadline = Instant::now() + Duration::from_secs(5);
        while app.editor.monitor_frame_for_layer(0).is_none() && Instant::now() < lower_deadline {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            app.editor
                .monitor_frame_for_layer(0)
                .and_then(|frame| frame.media_id),
            Some(1)
        );
        let errors_before_takeover = app.runtime_diagnostics().monitor_errors;

        let mut both = preview_request(&app.editor);
        both.is_scrubbing = true;
        both.output_size = [64, 36];
        app.submit_monitor_decode_request(both);

        let upper_deadline = Instant::now() + Duration::from_secs(5);
        while app.editor.monitor_frame_for_layer(1).is_none() && Instant::now() < upper_deadline {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }

        assert!(app.monitor_request_deferred[0]);
        assert!(
            !app.monitor_request_deferred[1],
            "top source must take the yielded one-source permit"
        );
        assert_eq!(
            app.editor
                .monitor_frame_for_layer(1)
                .and_then(|frame| frame.media_id),
            Some(2),
            "top source must eventually decode after the old session permit retires"
        );
        assert_eq!(
            app.runtime_diagnostics().monitor_errors,
            errors_before_takeover,
            "active takeover must not turn transient permit pressure into an error"
        );
        let source_diagnostics = app.monitor_source_coordinator.diagnostics();
        assert_eq!(source_diagnostics.live_source_groups, 1);
        assert!(
            source_diagnostics.live_lane_actors + source_diagnostics.retiring_lane_actors
                <= source_diagnostics.lane_actor_cap
        );
        let audio_after = app
            .editor
            .audio_playback_targets()
            .into_iter()
            .map(|target| {
                (
                    target.track_id,
                    target.clip_id,
                    target.media_id,
                    target.path.to_path_buf(),
                    target.source_tick,
                    target.clip_tick,
                    target.gain_db.to_bits(),
                    target.pan.to_bits(),
                    target.transition,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(audio_after, audio_before);
        drop(app);
        fs::remove_dir_all(fixture_root).expect("remove priority fixture directory");
    }

    fn wait_for_project_open(app: &mut App) {
        for _ in 0..200 {
            app.poll_project_dialog();
            if app.hub.status.as_deref() != Some("Opening project…") {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("project reader did not finish");
    }

    #[test]
    fn splash_never_allows_continuing_before_resources_are_ready() {
        assert!(!splash_can_continue(false, Duration::from_secs(60)));
    }

    #[test]
    fn splash_only_allows_continuing_after_the_minimum_presentation() {
        assert!(!splash_can_continue(
            true,
            MIN_SPLASH_VISIBLE - Duration::from_millis(1)
        ));
        assert!(splash_can_continue(true, MIN_SPLASH_VISIBLE));
    }

    #[test]
    fn splash_loading_line_follows_startup_stages() {
        assert_eq!(
            splash_load_stage(false, false, false, false),
            SplashLoadStage::Graphics
        );
        assert_eq!(
            splash_load_stage(true, false, false, false),
            SplashLoadStage::Library
        );
        assert_eq!(
            splash_load_stage(true, true, false, false),
            SplashLoadStage::Audio
        );
        assert_eq!(
            splash_load_stage(true, true, true, false),
            SplashLoadStage::Holding
        );
        assert_eq!(
            splash_load_stage(true, true, true, true),
            SplashLoadStage::Ready
        );
        assert_eq!(
            splash_load_copy(Language::English, SplashLoadStage::Audio),
            "LOADING  AUDIO ENGINE"
        );
    }

    #[test]
    fn splash_readiness_requires_hardware_catalog_and_audio() {
        assert!(!startup_resources_are_ready(false, true, true));
        assert!(!startup_resources_are_ready(true, false, true));
        assert!(!startup_resources_are_ready(true, true, false));
        assert!(startup_resources_are_ready(true, true, true));
    }

    #[test]
    fn startup_resource_bundle_retains_manifest_models_before_becoming_ready() {
        let root = test_catalog_path("startup-model-bundle")
            .parent()
            .unwrap()
            .join("models");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("fixture.bin"), [4, 3, 2, 1]).unwrap();
        fs::write(
            root.join("manifest.json"),
            br#"{"version":1,"models":[{"id":"fixture","file":"fixture.bin","expected_bytes":4}]}"#,
        )
        .unwrap();

        let resources = load_startup_resources_from(None, Some(&root));
        assert!(resources.model_errors.is_empty());
        assert_eq!(resources.preloaded_models.len(), 1);
        assert_eq!(resources.preloaded_models.total_bytes(), 4);
        assert_eq!(
            resources.preloaded_models.get("fixture").as_deref(),
            Some([4, 3, 2, 1].as_slice())
        );
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn startup_probe_records_only_the_first_present_from_app_construction() {
        let started_at = Instant::now();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut probe = StartupPresentationProbe {
            started_at,
            report_tx: Some(tx),
        };
        probe.record_first_present(started_at + Duration::from_millis(125));
        probe.record_first_present(started_at + Duration::from_secs(1));

        let report = rx.try_recv().expect("first present report");
        assert!((report.first_surface_present_ms - 125.0).abs() < 0.001);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn embedded_window_icon_is_purpose_sized_and_splash_rgba_is_reusable() {
        let icon = image::load_from_memory(APP_ICON).expect("embedded window icon");
        assert_eq!((icon.width(), icon.height()), (256, 256));

        let english = decode_embedded_rgba(ENGLISH_SPLASH);
        let japanese = decode_embedded_rgba(JAPANESE_SPLASH);
        assert_eq!((english.width, english.height), (1_672, 941));
        assert_eq!((japanese.width, japanese.height), (1_672, 941));
        assert_eq!(english.rgba.len(), 1_672 * 941 * 4);
        assert_eq!(japanese.rgba.len(), 1_672 * 941 * 4);
    }

    #[test]
    fn project_switch_invalidates_queued_analysis_but_keeps_global_worker_bound() {
        let mut app = App::new_with_catalog(true, None);
        let old_epoch = app.media_analysis_epoch;
        app.media_analysis_pending
            .push_back((old_epoch, 1, PathBuf::from("old-project.mp4")));
        app.media_analysis_in_flight.insert((old_epoch, 1));
        app.media_analysis_in_flight.insert((old_epoch, 2));
        let cancel_finished = Arc::new(AtomicBool::new(false));
        let cancel_active = Arc::new(AtomicBool::new(false));
        app.media_analysis_cancellations
            .insert((old_epoch, 1), Arc::clone(&cancel_finished));
        app.media_analysis_cancellations
            .insert((old_epoch, 2), Arc::clone(&cancel_active));
        app.media_analysis_tx
            .send(MediaAnalysisResult {
                project_epoch: old_epoch,
                media_id: 1,
                is_still: false,
                metadata: Err("finished old metadata".to_owned()),
                frame_timing: Err("finished old timing".to_owned()),
                waveform: Err("finished old waveform".to_owned()),
                video_strip: Err("finished old strip".to_owned()),
            })
            .expect("queue completed old analysis result");

        app.reset_media_analysis_session();

        assert_ne!(app.media_analysis_epoch, old_epoch);
        assert!(app.media_analysis_pending.is_empty());
        assert!(!app.media_analysis_in_flight.contains(&(old_epoch, 1)));
        assert!(!app.media_analysis_in_flight.contains(&(old_epoch, 2)));
        assert!(cancel_finished.load(Ordering::Acquire));
        assert!(cancel_active.load(Ordering::Acquire));
        assert!(
            !app.media_analysis_cancellations
                .contains_key(&(old_epoch, 1))
        );
        assert!(app.media_analysis_cancellations.is_empty());
    }

    #[test]
    fn stale_monitor_frames_are_rejected_by_epoch_and_request() {
        assert!(monitor_event_is_current(4, 9, 4, 9));
        assert!(!monitor_event_is_current(4, 9, 3, 9));
        assert!(!monitor_event_is_current(4, 9, 4, 8));
    }

    #[test]
    fn runtime_diagnostics_classify_monitor_events_without_a_native_viewer() {
        let frame = |project_epoch, request_id, source_tick| {
            nle_decode::DecodeEvent::Frame(nle_decode::DecodedFrame {
                project_epoch,
                request_id,
                media_id: 1,
                source_tick,
                width: 1,
                height: 1,
                backend: Some(nle_decode::DecodeBackend::Software),
                fallback_reason: Some(nle_decode::DecodeFallbackReason::ForcedSoftware),
                rgba: Arc::from([0, 0, 0, 255]),
            })
        };
        let mut app = App::new_without_startup_or_audio_for_monitor_contract();
        assert!(app.audio_engine.is_none());
        assert!(!app.audio_engine_initialized);
        assert!(app.startup_resources_tx.is_none());
        assert!(!app.startup_resources_ready);
        assert_eq!(app.preloaded_models.len(), 0);
        app.editor
            .add_media_paths([PathBuf::from("runtime-counter-contract.mp4")]);
        let video_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .expect("default video track")
            .id;
        app.editor
            .timeline
            .insert_clip(
                video_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(10_000),
                nle_timeline::Tick(0),
            )
            .expect("insert one monitor clip");
        app.editor.set_playhead(nle_timeline::Tick(100));
        assert_eq!(app.editor.playback_targets().count(), 1);

        let epoch = 7;
        app.monitor_generations[0] = epoch;
        let install_request = |app: &mut App, request_id: u64, source_tick: i64| {
            app.record_monitor_request_submission(
                0,
                MonitorRequestKey {
                    project_epoch: epoch,
                    media_id: 1,
                    source_tick,
                    width: 1,
                    height: 1,
                    is_scrubbing: false,
                    prewarm_scrub_workers: false,
                    high_quality_scaling: false,
                    selected_quality: PreviewQuality::Full,
                    resolved_quality: PreviewQuality::Full,
                    source_frame_rate: None,
                    source_frame_duration_tick: None,
                },
                MonitorSourceIdentity {
                    media_id: 1,
                    path: PathBuf::from("runtime-counter-contract.mp4"),
                    acceleration: nle_decode::AccelerationPreference::Software,
                },
                request_id,
                false,
            );
        };
        let mut adaptive_quality_changed = false;

        install_request(&mut app, 10, 100);
        app.editor.set_monitor_frame_for_layer(
            0,
            egui::TextureId::Managed(900),
            1,
            1,
            Some(1),
            Some(nle_timeline::Tick(100)),
        );
        // A cancelled generation must be dropped before source or convergence work.
        assert!(!app.apply_monitor_decode_event(
            0,
            frame(epoch - 1, 10, 100),
            &mut adaptive_quality_changed,
        ));
        // An older request in the current generation cannot replace a converged retained frame.
        assert!(!app.apply_monitor_decode_event(
            0,
            frame(epoch, 9, 0),
            &mut adaptive_quality_changed,
        ));

        // The current request arrives late while a prior frame is retained: present and record hold.
        app.monitor_request_started_at[0] = Some((
            10,
            Instant::now()
                .checked_sub(Duration::from_millis(100))
                .expect("100ms precedes now"),
        ));
        assert!(app.apply_monitor_decode_event(
            0,
            frame(epoch, 10, 100),
            &mut adaptive_quality_changed
        ));

        // The next late completion has no retained frame, so it cannot record a hold.
        app.editor.reset_monitor_layer(0);
        install_request(&mut app, 11, 200);
        app.monitor_request_started_at[0] = Some((
            11,
            Instant::now()
                .checked_sub(Duration::from_millis(100))
                .expect("100ms precedes now"),
        ));
        assert!(app.apply_monitor_decode_event(
            0,
            frame(epoch, 11, 200),
            &mut adaptive_quality_changed
        ));

        // A current completion without a turnaround sample still presents and adds no late frame.
        install_request(&mut app, 12, 300);
        app.monitor_request_started_at[0] = None;
        assert!(app.apply_monitor_decode_event(
            0,
            frame(epoch, 12, 300),
            &mut adaptive_quality_changed
        ));

        install_request(&mut app, 13, 300);
        assert!(!app.apply_monitor_decode_event(
            0,
            nle_decode::DecodeEvent::Error(nle_decode::DecodeError {
                project_epoch: epoch,
                request_id: 13,
                media_id: 1,
                source_tick: 300,
                message: "synthetic current decode failure".to_owned(),
            }),
            &mut adaptive_quality_changed,
        ));

        let diagnostics = app.runtime_diagnostics();
        assert_eq!(diagnostics.monitor_requests, 4);
        assert_eq!(diagnostics.monitor_completed_frames, 3);
        assert_eq!(diagnostics.monitor_presented_frames, 3);
        assert_eq!(diagnostics.monitor_dropped_frames, 2);
        assert_eq!(diagnostics.monitor_hold_events, 1);
        assert_eq!(diagnostics.monitor_late_frames, 2);
        assert_eq!(diagnostics.monitor_errors, 1);
        assert!(diagnostics.monitor_turnaround_p95_ms >= 100.0);
        // A headless App has no HubRenderer, so this proves the observed fallback path rather
        // than claiming a native GPU upload that did not occur.
        assert_eq!(diagnostics.native_viewer_uploads, 0);
        assert_eq!(diagnostics.fallback_viewer_uploads, 3);
        assert_eq!(diagnostics.audio_underrun_frames, 0);
        assert_eq!(diagnostics.audio_callback_lock_failures, 0);
        assert_eq!(diagnostics.audio_late_discarded_frames, 0);
        assert!(diagnostics.monitor_hold_events <= diagnostics.monitor_late_frames);
        assert_eq!(
            diagnostics.monitor_presented_frames,
            diagnostics.native_viewer_uploads + diagnostics.fallback_viewer_uploads
        );
        assert_eq!(
            app.editor.active_preview_diagnostic_for_layer(0),
            Some(ActivePreviewDiagnostic::new(
                1,
                ActivePreviewSourceKind::OriginalSource,
                Some(ActivePreviewDecoderBackend::Software),
                Some(ActivePreviewFallbackReason::ForcedSoftware),
                PreviewQuality::Full,
                PreviewQuality::Full,
                [1, 1],
            ))
        );

        // Shared-cache pixels do not retain producer provenance. A cache hit must replace—not
        // inherit—the prior concrete software/fallback diagnosis.
        install_request(&mut app, 14, 400);
        assert!(app.apply_monitor_decode_event(
            0,
            nle_decode::DecodeEvent::Frame(nle_decode::DecodedFrame {
                project_epoch: epoch,
                request_id: 14,
                media_id: 1,
                source_tick: 400,
                width: 1,
                height: 1,
                backend: None,
                fallback_reason: None,
                rgba: Arc::from([0, 0, 0, 255]),
            }),
            &mut adaptive_quality_changed,
        ));
        assert_eq!(
            app.editor.active_preview_diagnostic_for_layer(0),
            Some(ActivePreviewDiagnostic::new(
                1,
                ActivePreviewSourceKind::OriginalSource,
                None,
                None,
                PreviewQuality::Full,
                PreviewQuality::Full,
                [1, 1],
            ))
        );
    }

    #[test]
    fn monitor_convergence_accepts_initial_frame_without_displayed_source() {
        assert!(monitor_frame_converges_to_target(None, 1_000, 700, false));
    }

    #[test]
    fn monitor_convergence_accepts_forward_progress_and_rejects_preroll_reset() {
        assert!(monitor_frame_converges_to_target(
            Some(700),
            1_000,
            850,
            false
        ));
        assert!(!monitor_frame_converges_to_target(
            Some(850),
            1_000,
            700,
            false
        ));
    }

    #[test]
    fn monitor_convergence_accepts_reverse_progress_and_rejects_forward_replay() {
        assert!(monitor_frame_converges_to_target(
            Some(1_300),
            1_000,
            1_150,
            false
        ));
        assert!(!monitor_frame_converges_to_target(
            Some(1_150),
            1_000,
            1_300,
            false
        ));
    }

    #[test]
    fn monitor_convergence_rejects_equal_distance_oscillation() {
        assert!(!monitor_frame_converges_to_target(
            Some(900),
            1_000,
            1_100,
            false
        ));
    }

    #[test]
    fn monitor_convergence_accepts_exact_target() {
        assert!(monitor_frame_converges_to_target(
            Some(1_000),
            1_000,
            1_000,
            true
        ));
    }

    #[test]
    fn monitor_convergence_accepts_completed_latest_frame_after_target() {
        assert!(monitor_frame_converges_to_target(
            Some(999),
            1_000,
            1_033,
            true
        ));
    }

    #[test]
    fn monitor_completion_waits_for_latest_request_to_reach_target() {
        assert!(monitor_frame_completes_request(9, Some(1_000), 9, 999));
        assert!(!monitor_frame_completes_request(9, Some(1_000), 9, 998));
        assert!(monitor_frame_completes_request(9, Some(1_000), 9, 1_033));
        assert!(!monitor_frame_completes_request(9, Some(1_000), 8, 1_033));
    }

    #[test]
    fn rounded_final_monitor_frame_completes_only_the_current_request() {
        let frame = |project_epoch, request_id, source_tick| {
            nle_decode::DecodeEvent::Frame(nle_decode::DecodedFrame {
                project_epoch,
                request_id,
                media_id: 1,
                source_tick,
                width: 1,
                height: 1,
                backend: Some(nle_decode::DecodeBackend::Software),
                fallback_reason: Some(nle_decode::DecodeFallbackReason::ForcedSoftware),
                rgba: Arc::from([0, 0, 0, 255]),
            })
        };
        let mut app = App::new_without_startup_or_audio_for_monitor_contract();
        app.editor
            .add_media_paths([PathBuf::from("rounded-monitor-completion.mp4")]);
        let video_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .expect("default video track")
            .id;
        app.editor
            .timeline
            .insert_clip(
                video_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .expect("insert monitor clip");
        app.editor.set_playhead(nle_timeline::Tick(1_433_334));

        let epoch = 7;
        let request_id = 10;
        // The live artifact requested 1,433,334 µs but FFmpeg returned the same rational
        // frame at 1,433,333 µs after rescaling.
        let target_source_tick = 1_433_334;
        app.monitor_generations[0] = epoch;
        app.record_monitor_request_submission(
            0,
            MonitorRequestKey {
                project_epoch: epoch,
                media_id: 1,
                source_tick: target_source_tick,
                width: 1,
                height: 1,
                is_scrubbing: false,
                prewarm_scrub_workers: false,
                high_quality_scaling: false,
                selected_quality: PreviewQuality::Full,
                resolved_quality: PreviewQuality::Full,
                source_frame_rate: None,
                source_frame_duration_tick: None,
            },
            MonitorSourceIdentity {
                media_id: 1,
                path: PathBuf::from("rounded-monitor-completion.mp4"),
                acceleration: nle_decode::AccelerationPreference::Software,
            },
            request_id,
            true,
        );
        let mut adaptive_quality_changed = false;

        assert!(!app.apply_monitor_decode_event(
            0,
            frame(epoch - 1, request_id, target_source_tick - 1),
            &mut adaptive_quality_changed,
        ));
        assert!(app.monitor_requests_in_flight[0]);
        assert!(app.monitor_request_deferred[0]);

        assert!(app.apply_monitor_decode_event(
            0,
            frame(epoch, request_id - 1, target_source_tick - 1),
            &mut adaptive_quality_changed,
        ));
        assert!(app.monitor_requests_in_flight[0]);
        assert!(app.monitor_request_deferred[0]);

        assert!(!app.apply_monitor_decode_event(
            0,
            frame(epoch, request_id, target_source_tick - 2),
            &mut adaptive_quality_changed,
        ));
        assert!(app.monitor_requests_in_flight[0]);
        assert!(app.monitor_request_deferred[0]);
        assert_eq!(app.runtime_diagnostics().monitor_completed_frames, 0);

        assert!(app.apply_monitor_decode_event(
            0,
            frame(epoch, request_id, target_source_tick - 1),
            &mut adaptive_quality_changed,
        ));
        assert!(!app.monitor_requests_in_flight[0]);
        assert!(!app.monitor_request_deferred[0]);
        assert_eq!(app.runtime_diagnostics().monitor_completed_frames, 1);
        assert_eq!(
            app.editor
                .monitor_frame_for_layer(0)
                .and_then(|frame| frame.source_tick)
                .map(|tick| tick.0),
            Some(target_source_tick - 1)
        );
    }

    #[test]
    fn catalog_round_trip_uses_explicit_temp_path() {
        let path = test_catalog_path("round-trip");
        let projects = vec![project(7, "Persistent Cut", "Just now")];
        persist_catalog(&path, &projects).expect("write catalog");
        assert_eq!(load_catalog(&path), projects);
        fs::remove_dir_all(path.parent().expect("test catalog parent")).expect("remove test data");
    }

    #[test]
    fn project_document_round_trip_and_backup_recovery_use_explicit_temp_path() {
        let catalog = test_catalog_path("project-document");
        let path = project_document_path(&catalog, 42);
        let snapshot = EditorState::new(Language::English, "Round trip").snapshot();
        persist_project_document(&SaveRequest {
            project_path: path.clone(),
            document: test_document(&path, snapshot.clone()),
            thumbnail: None,
        })
        .expect("write project document");
        assert_eq!(
            load_project_document(&path)
                .expect("load document")
                .expect("document")
                .snapshot,
            snapshot
        );

        let replacement = EditorState::new(Language::Japanese, "Backup").snapshot();
        persist_project_document(&SaveRequest {
            project_path: path.clone(),
            document: test_document(&path, replacement.clone()),
            thumbnail: None,
        })
        .expect("write replacement document");
        let mut invalid_snapshot = replacement;
        invalid_snapshot.version = 0;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&test_document(&path, invalid_snapshot))
                .expect("serialize semantically invalid primary"),
        )
        .expect("write semantically invalid primary");
        assert_eq!(
            load_project_document(&path)
                .expect("recover backup")
                .expect("backup")
                .snapshot,
            snapshot
        );
        fs::remove_dir_all(catalog.parent().expect("test root")).expect("remove test data");
    }

    #[test]
    fn failed_writer_request_can_be_retried_after_storage_recovers() {
        let catalog = test_catalog_path("writer-retry");
        let blocker = catalog.parent().expect("test root").join("blocked-parent");
        fs::create_dir_all(blocker.parent().expect("blocker parent")).expect("create test root");
        fs::write(&blocker, b"not a directory").expect("create path blocker");
        let path = blocker.join("project.json");
        let snapshot = EditorState::new(Language::English, "Retry").snapshot();
        let notified = Arc::new(AtomicBool::new(false));
        let worker_notified = Arc::clone(&notified);
        let mut writer = ProjectWriter::new_with_notifier(move || {
            worker_notified.store(true, Ordering::Release);
        });
        writer.save_latest(SaveRequest {
            project_path: path.clone(),
            document: test_document(&path, snapshot.clone()),
            thumbnail: None,
        });
        writer.flush();
        let failure = writer
            .error_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer reports failed request");
        assert!(notified.load(Ordering::Acquire));
        assert_eq!(failure.request.project_path, path);

        fs::remove_file(&blocker).expect("remove path blocker");
        // The worker retains a failed request. A flush after the storage repair retries it
        // without reconstructing the save from whichever project happens to be open now.
        writer.flush();
        assert_eq!(
            load_project_document(&path)
                .expect("load retried document")
                .expect("retried document")
                .snapshot,
            snapshot
        );
        writer.flush_and_shutdown();
        fs::remove_dir_all(catalog.parent().expect("test root")).expect("remove test data");
    }

    #[test]
    fn final_app_flush_drains_failure_and_retries_current_snapshot() {
        let blocked_catalog = test_catalog_path("app-flush-blocked");
        let blocker = blocked_catalog
            .parent()
            .expect("blocked catalog parent")
            .to_path_buf();
        fs::write(&blocker, b"not a directory").expect("create catalog path blocker");
        let mut app = App::new_with_catalog(true, Some(blocked_catalog));
        app.current_project_id = Some(17);
        app.editor = EditorState::new(Language::English, "Final flush");
        app.queue_project_autosave();
        app.project_writer.flush();

        let recovered_catalog = test_catalog_path("app-flush-recovered");
        app.catalog_path = Some(recovered_catalog.clone());
        app.flush_project_autosave();
        assert_eq!(
            load_project_document(&project_document_path(&recovered_catalog, 17))
                .expect("load final flush")
                .expect("final flush document")
                .snapshot,
            app.editor.snapshot()
        );

        drop(app);
        fs::remove_file(&blocker).expect("remove path blocker");
        fs::remove_dir_all(
            recovered_catalog
                .parent()
                .expect("recovered catalog parent"),
        )
        .expect("remove recovered test data");
    }

    #[test]
    fn autosave_writer_coalesces_to_latest_snapshot() {
        let catalog = test_catalog_path("autosave");
        let path = project_document_path(&catalog, 9);
        let first = EditorState::new(Language::English, "First").snapshot();
        let mut latest = EditorState::new(Language::English, "Latest").snapshot();
        latest.view.playhead = nle_timeline::Tick(123_456);
        let mut writer = ProjectWriter::new();
        for snapshot in [first, latest.clone()] {
            writer.save_latest(SaveRequest {
                project_path: path.clone(),
                document: test_document(&path, snapshot),
                thumbnail: None,
            });
        }
        writer.flush();
        assert_eq!(
            load_project_document(&path)
                .expect("load latest")
                .expect("document")
                .snapshot,
            latest
        );
        writer.flush_and_shutdown();
        fs::remove_dir_all(catalog.parent().expect("test root")).expect("remove test data");
    }

    #[test]
    fn autosave_writer_preserves_latest_snapshot_for_each_project() {
        let catalog = test_catalog_path("autosave-multiple-projects");
        let first_path = project_document_path(&catalog, 1);
        let second_path = project_document_path(&catalog, 2);
        let first_latest = EditorState::new(Language::English, "First latest").snapshot();
        let second_latest = EditorState::new(Language::Japanese, "Second latest").snapshot();
        let mut writer = ProjectWriter::new();

        for (path, snapshot) in [
            (
                first_path.clone(),
                EditorState::new(Language::English, "First old").snapshot(),
            ),
            (second_path.clone(), second_latest.clone()),
            (first_path.clone(), first_latest.clone()),
        ] {
            writer.save_latest(SaveRequest {
                project_path: path.clone(),
                document: test_document(&path, snapshot),
                thumbnail: None,
            });
        }
        writer.flush();

        assert_eq!(
            load_project_document(&first_path)
                .expect("load first")
                .expect("first document")
                .snapshot,
            first_latest
        );
        assert_eq!(
            load_project_document(&second_path)
                .expect("load second")
                .expect("second document")
                .snapshot,
            second_latest
        );
        writer.flush_and_shutdown();
        fs::remove_dir_all(catalog.parent().expect("test root")).expect("remove test data");
    }

    #[test]
    fn catalog_writer_coalesces_to_latest_snapshot() {
        let path = test_catalog_path("catalog-writer-latest");
        let mut writer = CatalogWriter::new();
        for name in ["First", "Latest"] {
            writer.save_latest(CatalogSaveRequest {
                path: path.clone(),
                catalog: ProjectCatalog {
                    version: PROJECT_CATALOG_VERSION,
                    projects: vec![CatalogProject::from(&project(1, name, "Just now"))],
                },
            });
        }
        writer.flush();

        assert_eq!(load_catalog(&path)[0].name, "Latest");
        writer.flush_and_shutdown();
        fs::remove_dir_all(path.parent().expect("test catalog parent")).expect("remove test data");
    }

    #[cfg(windows)]
    #[test]
    fn forced_termination_autosave_helper() {
        let Some(project_path) = std::env::var_os("MAELSTROM_CRASH_TEST_PROJECT") else {
            return;
        };
        let Some(ready_path) = std::env::var_os("MAELSTROM_CRASH_TEST_READY") else {
            return;
        };
        let project_path = PathBuf::from(project_path);
        let ready_path = PathBuf::from(ready_path);
        let mut snapshot = EditorState::new(Language::English, "Recovered after crash").snapshot();
        snapshot.view.playhead = nle_timeline::Tick(7_654_321);
        let expected = snapshot.clone();
        let writer = ProjectWriter::new();
        writer.save_latest(SaveRequest {
            project_path: project_path.clone(),
            document: nle_project_io::document_for_path(
                &project_path,
                "Recovered after crash",
                snapshot,
                ProjectSettings::default(),
            ),
            thumbnail: None,
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if load_project_document(&project_path)
                .ok()
                .flatten()
                .is_some_and(|document| document.snapshot == expected)
            {
                fs::write(&ready_path, b"autosave durable").expect("signal durable autosave");
                // The parent must terminate us. Reaching normal Drop would invalidate the test.
                loop {
                    thread::sleep(Duration::from_secs(1));
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("background autosave did not become durable before timeout");
    }

    #[cfg(windows)]
    #[test]
    fn forced_process_termination_recovers_latest_background_autosave() {
        struct KillOnDrop(std::process::Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let catalog = test_catalog_path("forced-termination");
        let root = catalog.parent().expect("test root");
        fs::create_dir_all(root).expect("create crash test root");
        let project_path = project_document_path(&catalog, 42);
        let ready_path = root.join("autosave-ready");

        let mut child = KillOnDrop(
            std::process::Command::new(std::env::current_exe().expect("test exe"))
                .arg("--exact")
                .arg("tests::forced_termination_autosave_helper")
                .arg("--test-threads=1")
                .env("MAELSTROM_CRASH_TEST_PROJECT", &project_path)
                .env("MAELSTROM_CRASH_TEST_READY", &ready_path)
                .spawn()
                .expect("start autosave child process"),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready_path.exists() && Instant::now() < deadline {
            assert!(
                child.0.try_wait().expect("poll autosave child").is_none(),
                "autosave child exited before its save became durable"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready_path.exists(),
            "autosave child never reported durability"
        );
        child.0.kill().expect("forcibly terminate autosave child");
        let status = child.0.wait().expect("reap terminated autosave child");
        assert!(!status.success(), "child unexpectedly shut down cleanly");

        let recovered = load_project_document(&project_path)
            .expect("read project after forced termination")
            .expect("recover project after forced termination");
        assert_eq!(recovered.project_name, "Recovered after crash");
        assert_eq!(
            recovered.snapshot.view.playhead,
            nle_timeline::Tick(7_654_321)
        );
        fs::remove_dir_all(root).expect("remove crash test data");
    }

    #[test]
    fn coalesced_snapshot_keeps_an_unwritten_thumbnail() {
        let catalog = test_catalog_path("thumbnail-coalesce");
        let project_path = project_document_path(&catalog, 11);
        let thumbnail_path = project_thumbnail_path(&catalog, 11);
        let thumbnail = ThumbnailRgba {
            width: 1,
            height: 1,
            rgba: vec![4, 8, 15, 255],
        };
        let mut pending = HashMap::new();
        coalesce_save_request(
            &mut pending,
            SaveRequest {
                project_path: project_path.clone(),
                document: test_document(
                    &project_path,
                    EditorState::new(Language::English, "First").snapshot(),
                ),
                thumbnail: Some((thumbnail_path.clone(), thumbnail)),
            },
        );
        coalesce_save_request(
            &mut pending,
            SaveRequest {
                project_path: project_path.clone(),
                document: test_document(
                    &project_path,
                    EditorState::new(Language::English, "Latest").snapshot(),
                ),
                thumbnail: None,
            },
        );
        assert_eq!(
            pending
                .get(&project_path)
                .expect("coalesced request")
                .thumbnail
                .as_ref()
                .expect("thumbnail retained")
                .0,
            thumbnail_path
        );
    }

    #[test]
    fn failed_save_retention_never_replaces_a_newer_project_snapshot() {
        let catalog = test_catalog_path("failed-save-ordering");
        let project_path = project_document_path(&catalog, 12);
        let thumbnail_path = project_thumbnail_path(&catalog, 12);
        let latest = EditorState::new(Language::English, "Latest").snapshot();
        let mut pending = HashMap::from([(
            project_path.clone(),
            SaveRequest {
                project_path: project_path.clone(),
                document: test_document(&project_path, latest.clone()),
                thumbnail: None,
            },
        )]);
        retain_failed_save(
            &mut pending,
            SaveRequest {
                project_path: project_path.clone(),
                document: test_document(
                    &project_path,
                    EditorState::new(Language::English, "Stale").snapshot(),
                ),
                thumbnail: Some((
                    thumbnail_path.clone(),
                    ThumbnailRgba {
                        width: 1,
                        height: 1,
                        rgba: vec![1, 2, 3, 255],
                    },
                )),
            },
        );

        let retained = &pending[&project_path];
        assert_eq!(retained.document.snapshot, latest);
        assert_eq!(
            retained
                .thumbnail
                .as_ref()
                .expect("failed thumbnail retained")
                .0,
            thumbnail_path
        );
    }

    #[test]
    fn durable_generation_skips_playback_clock_but_tracks_persistent_edits() {
        let mut editor = EditorState::new(Language::English, "Playback");
        let saved_generation = editor.durable_generation();
        editor.start_playback();
        editor.advance_playback(Duration::from_millis(41));
        assert_eq!(editor.durable_generation(), saved_generation);

        editor.add_media_paths([PathBuf::from("timeline-edit.mp4")]);
        assert_ne!(editor.durable_generation(), saved_generation);
    }

    #[test]
    fn autosave_debounces_continuous_generations_but_not_forced_or_thumbnail_saves() {
        let origin = Instant::now();
        let mut schedule = AutosaveSchedule::default();

        assert!(schedule.ready(None, 1, false, false, origin));
        assert!(!schedule.ready(Some(1), 2, false, false, origin));
        assert_eq!(
            schedule.deadline(),
            Some(origin + AUTOSAVE_DEBOUNCE),
            "idle event loop should sleep until this one deadline"
        );
        assert!(!schedule.ready(
            Some(1),
            2,
            false,
            false,
            origin + AUTOSAVE_DEBOUNCE - Duration::from_millis(1),
        ));
        assert!(!schedule.ready(Some(1), 3, false, false, origin + AUTOSAVE_DEBOUNCE,));
        assert_eq!(
            schedule.deadline(),
            Some(origin + AUTOSAVE_DEBOUNCE * 2),
            "a newer drag update restarts rather than shortens the quiet period"
        );
        assert!(schedule.ready(Some(1), 3, false, false, origin + AUTOSAVE_DEBOUNCE * 2,));

        assert!(schedule.ready(Some(3), 4, true, false, origin));
        assert!(schedule.ready(Some(3), 4, false, true, origin));
    }

    #[test]
    fn representative_thumbnail_crops_the_middle_atlas_frame() {
        let mut rgba = vec![0; 4 * 4 * 4];
        for pixel in rgba.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        for y in 0..2 {
            for x in 2..4 {
                rgba[(y * 4 + x) * 4] = 2;
            }
        }
        let strip = nle_waveform::VideoStrip {
            width: 4,
            height: 4,
            rgba,
            duration_seconds: 3.0,
            frame_count: 3,
            frame_width: 2,
            frame_height: 2,
            columns: 2,
            rows: 2,
        };
        let thumbnail = crop_representative_frame(&strip).expect("crop frame");
        assert_eq!((thumbnail.width, thumbnail.height), (2, 2));
        assert!(
            thumbnail
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [2, 0, 0, 255])
        );
        assert!(
            crop_video_strip_frame(&strip, 3).is_none(),
            "frame_count rejects padded atlas cells"
        );
    }

    #[test]
    fn video_strip_crop_and_sampling_validate_boundaries() {
        let strip = nle_waveform::VideoStrip {
            width: 4,
            height: 4,
            rgba: vec![255; 4 * 4 * 4],
            duration_seconds: 4.0,
            frame_count: 4,
            frame_width: 2,
            frame_height: 2,
            columns: 2,
            rows: 2,
        };
        assert_eq!(video_strip_sample_tick(&strip, 0), Some(0));
        assert_eq!(video_strip_sample_tick(&strip, 3), Some(3_000_000));
        assert_eq!(video_strip_sample_tick(&strip, 4), None);
        assert_eq!(nearest_video_strip_frame_index(&strip, -1), Some(0));
        assert_eq!(nearest_video_strip_frame_index(&strip, 499_999), Some(0));
        assert_eq!(nearest_video_strip_frame_index(&strip, 500_000), Some(1));
        assert_eq!(nearest_video_strip_frame_index(&strip, 3_999_999), Some(3));
        assert_eq!(nearest_video_strip_frame_index(&strip, 9_000_000), Some(3));

        let invalid = nle_waveform::VideoStrip {
            rgba: vec![0; 3],
            ..strip
        };
        assert!(crop_video_strip_frame(&invalid, 0).is_none());
    }

    #[test]
    fn scrub_proxy_dedup_and_runtime_eviction_are_bounded() {
        assert!(!should_present_scrub_proxy(Some((7, 3)), (7, 3)));
        assert!(should_present_scrub_proxy(Some((7, 3)), (7, 4)));
        assert!(should_retain_close_full_monitor_frame(
            Some((7, 1_000_000, 640, 360)),
            7,
            1_033_334,
            (640, 360),
            Some(33_334),
        ));
        assert!(!should_retain_close_full_monitor_frame(
            Some((7, 1_000_000, 160, 90)),
            7,
            1_033_334,
            (640, 360),
            Some(33_334),
        ));
        assert!(!should_retain_close_full_monitor_frame(
            Some((7, 1_000_000, 640, 360)),
            7,
            1_066_669,
            (640, 360),
            Some(33_334),
        ));
        assert!(should_retain_close_full_monitor_frame(
            Some((7, 1_000_000, 640, 360)),
            7,
            1_000_000,
            (640, 360),
            None,
        ));
        assert!(!scrub_proxy_allows_monitor_frame(true, 11, 10));
        assert!(scrub_proxy_allows_monitor_frame(true, 11, 11));
        assert!(scrub_proxy_allows_monitor_frame(false, 11, 10));

        let mut app = App::new_with_catalog(false, None);
        for media_id in 1..=MAX_RUNTIME_VIDEO_STRIPS as u32 + 1 {
            app.retain_video_strip(
                media_id,
                Arc::new(nle_waveform::VideoStrip {
                    width: 1,
                    height: 1,
                    rgba: vec![0; 4],
                    duration_seconds: 1.0,
                    frame_count: 1,
                    frame_width: 1,
                    frame_height: 1,
                    columns: 1,
                    rows: 1,
                }),
            );
        }
        assert_eq!(app.video_strips.len(), MAX_RUNTIME_VIDEO_STRIPS);
        assert!(!app.video_strips.contains_key(&1));
        assert!(
            app.video_strips
                .contains_key(&(MAX_RUNTIME_VIDEO_STRIPS as u32 + 1))
        );
        assert!(app.video_strip_bytes <= MAX_RUNTIME_VIDEO_STRIP_BYTES);

        app.touch_video_strip(2);
        app.retain_video_strip(
            MAX_RUNTIME_VIDEO_STRIPS as u32 + 2,
            Arc::new(nle_waveform::VideoStrip {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
                duration_seconds: 1.0,
                frame_count: 1,
                frame_width: 1,
                frame_height: 1,
                columns: 1,
                rows: 1,
            }),
        );
        assert!(app.video_strips.contains_key(&2));
        assert!(!app.video_strips.contains_key(&3));

        app.present_scrub_proxy(0, 2, 0, PreviewQuality::Auto, PreviewQuality::Quarter);
        assert_eq!(
            app.editor.active_preview_diagnostic_for_layer(0),
            Some(ActivePreviewDiagnostic::new(
                2,
                ActivePreviewSourceKind::InternalScrubPreview,
                None,
                None,
                PreviewQuality::Auto,
                PreviewQuality::Quarter,
                [1, 1],
            ))
        );
    }

    #[test]
    fn still_image_analysis_builds_one_bounded_alpha_thumbnail() {
        let catalog = test_catalog_path("still-image-analysis");
        let directory = catalog.parent().expect("catalog directory");
        fs::create_dir_all(directory).expect("create still-image test directory");
        let path = directory.join("source.png");
        image::RgbaImage::from_pixel(640, 480, image::Rgba([12, 34, 56, 96]))
            .save(&path)
            .expect("save test still image");

        let analysis =
            analyze_still_image(&path, &AtomicBool::new(false)).expect("analyze still image");

        assert_eq!((analysis.source_width, analysis.source_height), (640, 480));
        assert_eq!((analysis.strip.width, analysis.strip.height), (120, 90));
        assert_eq!(analysis.strip.frame_count, 1);
        assert_eq!((analysis.strip.columns, analysis.strip.rows), (1, 1));
        assert_eq!(analysis.strip.rgba.len(), 120 * 90 * 4);
        assert!(
            analysis
                .strip
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [12, 34, 56, 96])
        );
        assert_eq!(
            analysis.strip.duration_seconds,
            nle_ui_core::DEFAULT_STILL_IMAGE_DURATION.0 as f64 / 1_000_000.0
        );

        fs::remove_dir_all(directory).expect("remove still-image test directory");
    }

    #[test]
    fn scrub_preview_sampling_is_dense_for_short_clips_and_bounded_for_long_ones() {
        assert_eq!(scrub_preview_frame_count(0.1), SCRUB_PREVIEW_MIN_FRAMES);
        assert_eq!(scrub_preview_frame_count(10.0), 300);
        assert_eq!(scrub_preview_frame_count(60.0), SCRUB_PREVIEW_MAX_FRAMES);
        let dense_bytes = 5120usize * 2880 * 4;
        assert_eq!(dense_bytes, 90 * 160 * 1024 * 4);
        assert!(dense_bytes <= 64 * 1024 * 1024);
    }

    #[test]
    fn scrub_cancellation_invalidates_a_racing_monitor_frame() {
        let mut app = App::new_with_catalog(false, None);
        let invalidated_layer = MONITOR_LAYER_COUNT - 1;
        app.monitor_latest_request_ids[invalidated_layer] = 7;
        app.monitor_next_request_id = 8;
        let old_generation = app.monitor_generations[invalidated_layer];
        let unaffected_generations = app.monitor_generations[..invalidated_layer].to_vec();
        app.invalidate_monitor_request(invalidated_layer);
        assert_ne!(app.monitor_generations[invalidated_layer], old_generation);
        assert_eq!(
            app.monitor_generations[..invalidated_layer],
            unaffected_generations
        );
        assert_eq!(app.monitor_latest_request_ids[invalidated_layer], 8);
        assert_eq!(app.monitor_next_request_id, 9);
        assert!(!monitor_event_is_current(
            app.monitor_generations[invalidated_layer],
            app.monitor_latest_request_ids[invalidated_layer],
            old_generation,
            7,
        ));
    }

    #[test]
    fn scrub_disable_reenable_accepts_only_the_newest_layer_request() {
        let frame = |project_epoch, request_id, media_id, source_tick| {
            nle_decode::DecodeEvent::Frame(nle_decode::DecodedFrame {
                project_epoch,
                request_id,
                media_id,
                source_tick,
                width: 1,
                height: 1,
                backend: Some(nle_decode::DecodeBackend::Software),
                fallback_reason: Some(nle_decode::DecodeFallbackReason::ForcedSoftware),
                rgba: Arc::from([0, 0, 0, 255]),
            })
        };
        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([
            PathBuf::from("lower-unaffected.mp4"),
            PathBuf::from("upper-toggle.mp4"),
        ]);
        let tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        let lower_clip = app
            .editor
            .timeline
            .insert_clip(
                tracks[0],
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(10_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        let upper_clip = app
            .editor
            .timeline
            .insert_clip(
                tracks[1],
                nle_timeline::MediaId(2),
                nle_timeline::Tick(0),
                nle_timeline::Tick(10_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        app.editor.set_playhead(nle_timeline::Tick(1_000_000));
        let mut forward = preview_request(&app.editor);
        forward.is_scrubbing = true;
        app.submit_monitor_decode_request(forward);
        let forward_epoch = app.monitor_generations[1];
        let forward_request = app.monitor_latest_request_ids[1];

        app.editor.set_playhead(nle_timeline::Tick(500_000));
        let mut backward = preview_request(&app.editor);
        backward.is_scrubbing = true;
        app.submit_monitor_decode_request(backward);
        let backward_request = app.monitor_latest_request_ids[1];
        assert_eq!(app.monitor_generations[1], forward_epoch);
        assert_ne!(backward_request, forward_request);
        let mut adaptive_quality_changed = false;

        let lower_request = app.monitor_last_requests[0];
        let lower_generation = app.monitor_generations[0];
        let lower_request_id = app.monitor_latest_request_ids[0];
        app.editor.set_monitor_frame_for_layer(
            0,
            egui::TextureId::Managed(900),
            1,
            1,
            Some(1),
            Some(nle_timeline::Tick(500_000)),
        );
        assert!(app.editor.set_timeline_clip_enabled(upper_clip, false));
        let mut disabled = preview_request(&app.editor);
        disabled.is_scrubbing = true;
        assert!(disabled.sources[1].is_none());
        app.submit_monitor_decode_request(disabled);
        assert!(app.monitor_last_requests[1].is_none());
        assert!(!app.monitor_requests_in_flight[1]);
        assert!(app.editor.monitor_frame_for_layer(1).is_none());
        assert_ne!(app.monitor_generations[1], forward_epoch);
        assert!(!app.apply_monitor_decode_event(
            1,
            frame(forward_epoch, forward_request, 2, 1_000_000),
            &mut adaptive_quality_changed,
        ));
        assert!(!app.apply_monitor_decode_event(
            1,
            frame(forward_epoch, backward_request, 2, 500_000),
            &mut adaptive_quality_changed,
        ));
        assert!(app.editor.monitor_frame_for_layer(1).is_none());
        assert_eq!(app.monitor_last_requests[0], lower_request);
        assert_eq!(app.monitor_generations[0], lower_generation);
        assert_eq!(app.monitor_latest_request_ids[0], lower_request_id);
        assert_eq!(
            app.editor.monitor_frame_for_layer(0).unwrap().texture,
            egui::TextureId::Managed(900)
        );

        assert!(app.editor.set_timeline_clip_enabled(upper_clip, true));
        app.editor.set_playhead(nle_timeline::Tick(2_000_000));
        let mut reenabled = preview_request(&app.editor);
        reenabled.is_scrubbing = true;
        app.submit_monitor_decode_request(reenabled);
        let newest_epoch = app.monitor_generations[1];
        let newest_request = app.monitor_latest_request_ids[1];
        let newest_key = app.monitor_last_requests[1].expect("re-enabled layer request");
        assert_eq!(newest_key.media_id, 2);
        assert_eq!(newest_key.source_tick, 2_000_000);
        assert!(!app.apply_monitor_decode_event(
            1,
            frame(forward_epoch, backward_request, 2, 500_000),
            &mut adaptive_quality_changed,
        ));
        assert!(app.apply_monitor_decode_event(
            1,
            frame(newest_epoch, newest_request, 2, newest_key.source_tick),
            &mut adaptive_quality_changed,
        ));
        assert_eq!(
            app.editor
                .monitor_frame_for_layer(1)
                .expect("newest layer frame is presented")
                .source_tick,
            Some(nle_timeline::Tick(2_000_000))
        );
        assert!(app.editor.timeline.clip(lower_clip).is_some());
    }

    #[test]
    fn shared_source_layers_reuse_one_session_and_release_after_last_consumer() {
        let fixture = test_catalog_path("shared-source-coordinator").with_extension("mp4");
        let Some(parent) = fixture.parent() else {
            return;
        };
        fs::create_dir_all(parent).expect("create shared-source fixture directory");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x36:rate=24",
                "-t",
                "1",
                "-an",
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
            ])
            .arg(&fixture)
            .output();
        let Ok(generated) = generated else {
            // Minimal developer environments may not include an FFmpeg CLI; decode behavior is
            // still covered by the pinned library tests, so this integration test skips cleanly.
            let _ = fs::remove_dir_all(parent);
            return;
        };
        assert!(
            generated.status.success(),
            "create shared-source fixture: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([fixture.clone()]);
        let tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(tracks.len(), 2);
        let lower = app
            .editor
            .timeline
            .insert_clip(
                tracks[0],
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(900_000),
                nle_timeline::Tick(0),
            )
            .expect("insert lower shared-source clip");
        let upper = app
            .editor
            .timeline
            .insert_clip(
                tracks[1],
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(900_000),
                nle_timeline::Tick(0),
            )
            .expect("insert upper shared-source clip");
        app.editor.set_playhead(nle_timeline::Tick(300_000));
        let mut preview = preview_request(&app.editor);
        preview.output_size = [64, 36];
        // Scrubbing does not prewarm speculative lanes, making the physical-session assertion
        // exact: two app layers, one foreground source actor/session.
        preview.is_scrubbing = true;
        app.submit_monitor_decode_request(preview);
        let request_ids = [
            app.monitor_latest_request_ids[0],
            app.monitor_latest_request_ids[1],
        ];
        assert!(request_ids.iter().all(|id| *id != 0));
        assert_ne!(request_ids[0], request_ids[1]);

        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < ready_deadline
            && (0..2).any(|layer| {
                app.editor.monitor_frame_for_layer(layer).is_none()
                    || app.monitor_requests_in_flight[layer]
            })
        {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }
        for layer in 0..2 {
            let frame = app
                .editor
                .monitor_frame_for_layer(layer)
                .expect("shared frame");
            assert_eq!(frame.media_id, Some(1));
            assert_eq!((frame.width, frame.height), (64, 36));
            assert!(!app.monitor_requests_in_flight[layer]);
        }
        let coordinator = app.monitor_source_coordinator.diagnostics();
        let sessions = app.monitor_session_pool.diagnostics();
        assert_eq!(coordinator.live_source_groups, 1);
        assert_eq!(coordinator.live_lane_actors, 1);
        assert_eq!(sessions.active_foreground_sessions, 1);
        assert_eq!(sessions.active_sticky_sessions, 1);
        let cached_before_release = app.monitor_frame_cache_pool.diagnostics().current_bytes;
        assert!(cached_before_release > 0);

        assert!(app.editor.set_timeline_clip_enabled(upper, false));
        let mut lower_only = preview_request(&app.editor);
        lower_only.output_size = [64, 36];
        lower_only.is_scrubbing = true;
        app.submit_monitor_decode_request(lower_only);
        assert!(app.monitor_last_requests[0].is_some());
        assert!(app.monitor_last_requests[1].is_none());
        assert_eq!(
            app.monitor_session_pool
                .diagnostics()
                .active_foreground_sessions,
            1
        );

        assert!(app.editor.set_timeline_clip_enabled(lower, false));
        let mut absent = preview_request(&app.editor);
        absent.output_size = [64, 36];
        absent.is_scrubbing = true;
        app.submit_monitor_decode_request(absent);
        let release_deadline = Instant::now() + Duration::from_secs(2);
        while app
            .monitor_session_pool
            .diagnostics()
            .active_sticky_sessions
            != 0
        {
            assert!(
                Instant::now() < release_deadline,
                "last shared-source consumer retained a sticky session"
            );
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            app.monitor_source_coordinator
                .diagnostics()
                .live_source_groups,
            0
        );
        assert_eq!(
            app.monitor_frame_cache_pool.diagnostics().current_bytes,
            cached_before_release,
            "releasing the final source lease must not clear the shared frame cache"
        );

        drop(app);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn playback_handoff_releases_paused_prewarm_session_before_foreground_decode() {
        let fixture = test_catalog_path("playback-handoff-releases-prewarm").with_extension("mp4");
        let Some(parent) = fixture.parent() else {
            return;
        };
        fs::create_dir_all(parent).expect("create playback handoff fixture directory");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x36:rate=24",
                "-t",
                "1",
                "-an",
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
            ])
            .arg(&fixture)
            .output();
        let Ok(generated) = generated else {
            let _ = fs::remove_dir_all(parent);
            return;
        };
        assert!(
            generated.status.success(),
            "create playback handoff fixture: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([fixture.clone()]);
        let video_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .expect("app includes a video track")
            .id;
        app.editor
            .timeline
            .insert_clip(
                video_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(900_000),
                nle_timeline::Tick(0),
            )
            .expect("insert playback handoff clip");
        app.editor.set_playhead(nle_timeline::Tick(300_000));

        let mut paused = preview_request(&app.editor);
        paused.output_size = [64, 36];
        assert!(!app.editor.playing && !paused.is_scrubbing);
        app.submit_monitor_decode_request(paused);
        let prewarm_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < prewarm_deadline {
            let sessions = app.monitor_session_pool.diagnostics();
            if sessions.active_foreground_sessions == 1 && sessions.active_background_sessions == 1
            {
                break;
            }
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }
        let prewarm_sessions = app.monitor_session_pool.diagnostics();
        assert_eq!(prewarm_sessions.active_foreground_sessions, 1);
        assert_eq!(prewarm_sessions.active_background_sessions, 1);
        let errors_before_handoff = app.runtime_diagnostics().monitor_errors;

        app.editor.start_playback();
        let mut playback = preview_request(&app.editor);
        playback.output_size = [64, 36];
        assert!(app.editor.playing && !playback.is_scrubbing);
        app.submit_monitor_decode_request(playback);
        assert!(
            !app.monitor_last_requests[0]
                .expect("playback foreground request")
                .prewarm_scrub_workers
        );

        let handoff_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < handoff_deadline {
            let sessions = app.monitor_session_pool.diagnostics();
            if sessions.active_foreground_sessions == 1
                && sessions.active_background_sessions == 0
                && !app.monitor_requests_in_flight[0]
            {
                break;
            }
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }
        let handoff_sessions = app.monitor_session_pool.diagnostics();
        assert_eq!(handoff_sessions.active_foreground_sessions, 1);
        assert_eq!(handoff_sessions.active_background_sessions, 0);
        assert_eq!(
            app.runtime_diagnostics().monitor_errors,
            errors_before_handoff,
            "releasing prewarm work must not report a monitor error"
        );

        drop(app);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn preview_request_captures_ordered_sources_and_resolved_output_quality() {
        let mut editor = EditorState::new(Language::English, "Preview request");
        editor.add_media_paths([
            PathBuf::from("lower-preview.mp4"),
            PathBuf::from("upper-preview.mp4"),
        ]);
        let video_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(MONITOR_LAYER_COUNT)
            .collect::<Vec<_>>();
        for (track, media_id) in video_tracks.into_iter().zip(1..=2) {
            editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(2_000_000),
                    nle_timeline::Tick(0),
                )
                .unwrap();
        }
        editor.set_playhead(nle_timeline::Tick(500_000));
        editor.playing = true;
        for (quality, expected_size) in [
            (PreviewQuality::Full, [640, 360]),
            (PreviewQuality::Half, [320, 180]),
            (PreviewQuality::Quarter, [160, 90]),
            (PreviewQuality::Eighth, [80, 45]),
        ] {
            editor.set_preview_quality(quality);
            let request = preview_request(&editor);
            assert_eq!(request.playhead_tick, 500_000);
            assert!(!request.is_scrubbing);
            assert_eq!(request.output_size, expected_size);
            assert_eq!(request.selected_quality, quality);
            assert_eq!(request.resolved_quality, quality);
            assert_eq!(request.sources[0].unwrap().media_id, 1);
            assert_eq!(request.sources[0].unwrap().priority, 1);
            assert_eq!(request.sources[1].unwrap().media_id, 2);
            assert_eq!(request.sources[1].unwrap().priority, 2);
        }
    }

    #[test]
    fn preview_request_preserves_indexed_vfr_boundaries_after_trim_and_slip() {
        let mut editor = EditorState::new(Language::English, "Indexed VFR preview request");
        editor.add_media_paths([PathBuf::from("indexed-vfr-preview.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .unwrap()
            .id;
        let clip_id = editor
            .timeline
            .insert_clip(
                video_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(1_000_000),
                nle_timeline::Tick(200_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        editor
            .timeline
            .trim_start(clip_id, nle_timeline::Tick(50_000), false, false)
            .unwrap();
        editor
            .timeline
            .slip_clip(clip_id, nle_timeline::Tick(50_000), false)
            .unwrap();
        editor.set_media_frame_time_index(
            1,
            Some(
                nle_ui_core::SourceFrameTimeIndex::new(vec![
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(40_000),
                    nle_timeline::Tick(110_000),
                    nle_timeline::Tick(150_000),
                    nle_timeline::Tick(240_000),
                    nle_timeline::Tick(310_000),
                ])
                .unwrap(),
            ),
        );

        for (playhead, decode_tick, duration) in [
            (1_050_000, 40_000, 70_000),
            (1_060_000, 110_000, 40_000),
            (1_060_001, 110_000, 40_000),
            (1_190_000, 240_000, 70_000),
            (1_200_000, 240_000, 70_000),
            (1_150_000, 150_000, 90_000),
            (1_060_001, 110_000, 40_000),
        ] {
            editor.set_playhead(nle_timeline::Tick(playhead));
            let source = preview_request(&editor).sources[0].unwrap();
            assert_eq!(source.source_tick, decode_tick);
            assert_eq!(source.source_frame_duration_tick, Some(duration));
            assert_eq!(source.source_frame_rate, None);
        }
    }

    #[test]
    fn preview_request_captures_ordered_audible_audio_metadata() {
        let mut editor = EditorState::new(Language::English, "Audio request");
        editor.add_media_paths([
            PathBuf::from("lower-audio-preview.mp4"),
            PathBuf::from("upper-audio-preview.mp4"),
        ]);
        let audio_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Audio)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        let lower = editor
            .timeline
            .insert_clip(
                audio_tracks[0],
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(100_000),
            )
            .unwrap();
        let upper = editor
            .timeline
            .insert_clip(
                audio_tracks[1],
                nle_timeline::MediaId(2),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(200_000),
            )
            .unwrap();
        editor.set_playhead(nle_timeline::Tick(500_000));

        let request = preview_request(&editor);
        assert_eq!(request.audio_source_count, 2);
        assert!(!request.audio_sources_truncated());
        let lower_request = request.audio_sources[0].unwrap();
        let upper_request = request.audio_sources[1].unwrap();
        assert_eq!(
            (
                lower_request.priority,
                lower_request.track_id,
                lower_request.clip_id
            ),
            (1, audio_tracks[0], lower)
        );
        assert_eq!(
            (
                lower_request.media_id,
                lower_request.source_tick,
                lower_request.clip_tick
            ),
            (1, 600_000, 500_000)
        );
        assert_eq!(lower_request.transition_role, None);
        assert_eq!(
            (
                upper_request.priority,
                upper_request.track_id,
                upper_request.clip_id
            ),
            (2, audio_tracks[1], upper)
        );
        assert_eq!(
            (
                upper_request.media_id,
                upper_request.source_tick,
                upper_request.clip_tick
            ),
            (2, 700_000, 500_000)
        );
        assert_eq!(upper_request.transition_role, None);
    }

    #[test]
    fn preview_request_captures_audio_transition_roles_in_playback_order() {
        let mut editor = EditorState::new(Language::English, "Audio transition request");
        editor.add_media_paths([
            PathBuf::from("outgoing-audio-transition.mp4"),
            PathBuf::from("incoming-audio-transition.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Audio)
            .unwrap()
            .id;
        let outgoing = editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(100_000),
            )
            .unwrap();
        let incoming = editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(2),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(200_000),
            )
            .unwrap();
        editor
            .timeline
            .add_audio_transition(track, outgoing, incoming, nle_timeline::Tick(1_000_000))
            .unwrap();
        editor.set_playhead(nle_timeline::Tick(2_000_000));

        let request = preview_request(&editor);
        assert_eq!(request.audio_source_count, 2);
        let outgoing_request = request.audio_sources[0].unwrap();
        let incoming_request = request.audio_sources[1].unwrap();
        assert_eq!(
            (outgoing_request.clip_id, outgoing_request.source_tick),
            (outgoing, 2_100_000)
        );
        assert_eq!(
            outgoing_request.transition_role,
            Some(nle_ui_core::AudioPlaybackTransitionRole::Outgoing)
        );
        assert_eq!(
            (incoming_request.clip_id, incoming_request.source_tick),
            (incoming, 200_000)
        );
        assert_eq!(
            incoming_request.transition_role,
            Some(nle_ui_core::AudioPlaybackTransitionRole::Incoming)
        );
    }

    #[test]
    fn preview_request_excludes_muted_and_non_solo_audio_tracks() {
        let mut editor = EditorState::new(Language::English, "Audible tracks request");
        editor.add_media_paths([PathBuf::from("audible-tracks.mp4")]);
        let audio_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Audio)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        for track in &audio_tracks {
            editor
                .timeline
                .insert_clip(
                    *track,
                    nle_timeline::MediaId(1),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(2_000_000),
                    nle_timeline::Tick(0),
                )
                .unwrap();
        }
        editor.set_playhead(nle_timeline::Tick(500_000));
        editor
            .timeline
            .set_track_muted(audio_tracks[0], true)
            .unwrap();
        let request = preview_request(&editor);
        assert_eq!(request.audio_source_count, 1);
        assert_eq!(request.audio_sources[0].unwrap().track_id, audio_tracks[1]);

        editor
            .timeline
            .set_track_muted(audio_tracks[0], false)
            .unwrap();
        editor
            .timeline
            .set_track_solo(audio_tracks[1], true)
            .unwrap();
        let request = preview_request(&editor);
        assert_eq!(request.audio_source_count, 1);
        assert_eq!(request.audio_sources[0].unwrap().track_id, audio_tracks[1]);
    }

    #[test]
    fn preview_request_reports_audio_metadata_truncation_without_capping_playback() {
        let mut editor = EditorState::new(Language::English, "Audio request capacity");
        editor.add_media_paths([PathBuf::from("many-audio-tracks.mp4")]);
        let mut audio_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Audio)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while audio_tracks.len() < MAX_PREVIEW_AUDIO_SOURCES + 1 {
            audio_tracks.push(editor.timeline.add_track(nle_timeline::TrackKind::Audio));
        }
        for track in &audio_tracks {
            editor
                .timeline
                .insert_clip(
                    *track,
                    nle_timeline::MediaId(1),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(2_000_000),
                    nle_timeline::Tick(0),
                )
                .unwrap();
        }
        editor.set_playhead(nle_timeline::Tick(500_000));

        let request = preview_request(&editor);
        assert_eq!(request.audio_source_count, MAX_PREVIEW_AUDIO_SOURCES + 1);
        assert!(request.audio_sources_truncated());
        assert_eq!(
            request.audio_sources.iter().flatten().count(),
            MAX_PREVIEW_AUDIO_SOURCES
        );
        assert_eq!(
            editor.audio_playback_targets().len(),
            MAX_PREVIEW_AUDIO_SOURCES + 1
        );
    }

    #[test]
    fn preview_request_uses_two_independent_slots_during_a_cross_dissolve() {
        let mut editor = EditorState::new(Language::English, "Transition request");
        editor.add_media_paths([
            PathBuf::from("outgoing-transition.mp4"),
            PathBuf::from("incoming-transition.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(2),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, nle_timeline::Tick(1_000_000), 0.0)
            .unwrap();
        editor.set_playhead(nle_timeline::Tick(2_000_000));

        let request = preview_request(&editor);
        let outgoing = request.sources[0].expect("outgoing transition source");
        let incoming = request.sources[1].expect("incoming transition source");
        assert_eq!((outgoing.clip_id, outgoing.media_id), (left, 1));
        assert_eq!((incoming.clip_id, incoming.media_id), (right, 2));
        assert_eq!(outgoing.source_tick, 3_000_000);
        assert_eq!(incoming.source_tick, 1_000_000);
        assert!(request.sources[2..].iter().all(Option::is_none));
    }

    #[test]
    fn preview_request_dip_to_black_decodes_only_the_visible_side_of_the_cut() {
        let mut editor = EditorState::new(Language::English, "Dip request");
        editor.add_media_paths([
            PathBuf::from("outgoing-dip.mp4"),
            PathBuf::from("incoming-dip.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(2),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition_of_kind(
                track,
                left,
                right,
                nle_timeline::Tick(1_000_000),
                0.0,
                nle_timeline::VideoTransitionKind::DipToBlack,
            )
            .unwrap();

        editor.set_playhead(nle_timeline::Tick(1_750_000));
        let request = preview_request(&editor);
        let outgoing = request.sources[0].expect("outgoing dip source");
        assert_eq!((outgoing.clip_id, outgoing.media_id), (left, 1));
        assert!(request.sources[1..].iter().all(Option::is_none));

        editor.set_playhead(nle_timeline::Tick(2_000_000));
        let request = preview_request(&editor);
        let incoming = request.sources[0].expect("incoming dip source");
        assert_eq!((incoming.clip_id, incoming.media_id), (right, 2));
        assert_eq!(incoming.source_tick, 0);
        assert!(request.sources[1..].iter().all(Option::is_none));
    }

    #[test]
    fn preview_request_keeps_still_image_decode_address_frozen() {
        let mut editor = EditorState::new(Language::English, "Still preview request");
        editor.add_media_paths([PathBuf::from("title-card.png")]);
        assert!(editor.add_selected_to_timeline());

        editor.set_playhead(nle_timeline::Tick(1_000_000));
        let first = preview_request(&editor).sources[0].expect("still preview source");
        editor.set_playhead(nle_timeline::Tick(4_000_000));
        let second = preview_request(&editor).sources[0].expect("still preview source");

        assert_eq!(first.media_id, second.media_id);
        assert_eq!(first.source_tick, 0);
        assert_eq!(second.source_tick, 0);
        assert_eq!(first.source_frame_rate, None);
        assert_eq!(second.source_frame_rate, None);
    }

    #[test]
    fn scrub_decode_targets_preserve_source_frame_precision_without_micro_seek_churn() {
        let editor = EditorState::new(Language::English, "Scrub target precision");
        let first = editor
            .frame_duration_tick()
            .0
            .saturating_mul(10)
            .saturating_add(1);
        // Model two different 60 fps source positions inside one 30 fps project frame.
        let second = first.saturating_add(editor.frame_duration_tick().0 / 2);

        assert_ne!(first, second);
        assert!(
            second < editor.frame_duration_tick().0.saturating_mul(11),
            "both samples must remain inside one project-frame interval"
        );
        let source_rate = nle_ui_core::SourceFrameRate::new(60, 1).unwrap();
        let first_source_frame = monitor_source_tick_for_preview(first, Some(source_rate));
        let second_source_frame = monitor_source_tick_for_preview(second, Some(source_rate));
        assert!(first_source_frame <= first && first - first_source_frame < 16_667);
        assert!(second_source_frame <= second && second - second_source_frame < 16_667);
        assert_ne!(
            first_source_frame, second_source_frame,
            "distinct 60 fps frames must not collapse to one 30 fps project frame"
        );
        assert_eq!(
            monitor_source_tick_for_preview(second, Some(source_rate)),
            monitor_source_tick_for_preview(second + 1_000, Some(source_rate)),
            "pointer samples inside one source frame must coalesce"
        );

        assert_eq!(
            monitor_source_tick_for_preview(first, Some(source_rate)),
            first_source_frame,
            "release refinement must retain the same source frame"
        );
        assert_eq!(
            monitor_source_tick_for_preview(second, Some(source_rate)),
            second_source_frame,
            "release refinement must not jump to a project-frame timestamp"
        );

        assert_eq!(
            monitor_source_tick_for_preview(second, None),
            second,
            "missing timing must not invent a fallback frame grid"
        );
        assert_eq!(
            monitor_source_tick_for_preview(-1, None),
            0,
            "missing timing must retain the nonnegative source tick"
        );
        assert_eq!(monitor_source_frame_duration_tick(None, None), None);
    }

    #[test]
    fn monitor_source_ticks_use_exact_ntsc_ratios_without_large_tick_drift() {
        let ntsc_30 = nle_ui_core::SourceFrameRate::new(30_000, 1_001).unwrap();
        let ntsc_60 = nle_ui_core::SourceFrameRate::new(60_000, 1_001).unwrap();
        assert_eq!(
            nle_ui_core::SourceFrameRate::new(60_000, 2_002),
            Some(ntsc_30),
            "equivalent rates must use one canonical request key"
        );
        assert_eq!(
            monitor_source_frame_duration_tick(Some(ntsc_30), None),
            Some(33_367)
        );
        assert_eq!(
            monitor_source_frame_duration_tick(Some(ntsc_60), None),
            Some(16_684)
        );
        assert_eq!(
            monitor_source_frame_duration_tick(Some(ntsc_30), Some(70_000)),
            Some(70_000),
            "an indexed VFR span must override the average probe rate"
        );
        assert_eq!(monitor_source_tick_for_preview(33_366, Some(ntsc_30)), 0);
        assert_eq!(
            monitor_source_tick_for_preview(33_367, Some(ntsc_30)),
            33_367,
            "a fractional NTSC boundary starts at its first representable microsecond"
        );
        assert_eq!(
            monitor_source_tick_for_preview(66_733, Some(ntsc_30)),
            33_367
        );
        assert_eq!(
            monitor_source_tick_for_preview(66_734, Some(ntsc_30)),
            66_734
        );

        let frame = 1_000_000_000_u128;
        let expected = (frame * 1_000_000 * 1_001).div_ceil(30_000) as i64;
        assert_eq!(
            monitor_source_tick_for_preview(expected, Some(ntsc_30)),
            expected
        );
        assert_eq!(
            monitor_source_tick_for_preview(expected.saturating_add(33_366), Some(ntsc_30)),
            expected,
            "large source ticks must retain the exact rational frame boundary"
        );

        let sixty_frame = 1_000_000_000_u128;
        let sixty_expected = (sixty_frame * 1_000_000 * 1_001).div_ceil(60_000) as i64;
        assert_eq!(
            monitor_source_tick_for_preview(sixty_expected, Some(ntsc_60)),
            sixty_expected
        );
    }

    #[test]
    fn supplied_vfr_fixture_routes_preview_to_complete_packet_boundaries() {
        let Some(path) = std::env::var_os("MAELSTROM_VFR_TEST_MEDIA").map(PathBuf::from) else {
            return;
        };
        let metadata =
            nle_waveform::probe_media_metadata(&path).expect("probe deterministic VFR fixture");
        let frame_timing =
            nle_waveform::analyze_frame_timing(&path).expect("scan deterministic VFR fixture");
        let nle_waveform::FrameTiming::Variable(index) = &frame_timing else {
            panic!("deterministic VFR fixture was not classified as variable");
        };
        assert_eq!(index.pts(), &[0, 40_000, 110_000, 150_000, 240_000]);

        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([path]);
        assert!(app.editor.add_selected_to_timeline());
        app.media_analysis_tx
            .send(MediaAnalysisResult {
                project_epoch: app.media_analysis_epoch,
                media_id: 1,
                is_still: false,
                metadata: Ok(metadata),
                frame_timing: Ok(frame_timing),
                waveform: Err("fixture intentionally has no audio".to_owned()),
                video_strip: Err("strip is irrelevant to timing proof".to_owned()),
            })
            .expect("queue VFR analysis");
        app.poll_media_analysis();

        for (logical_tick, expected_decode_tick, expected_duration) in [
            (0, 0, Some(40_000)),
            (1, 0, Some(40_000)),
            (40_000, 40_000, Some(70_000)),
            (40_001, 40_000, Some(70_000)),
            (240_001, 240_000, None),
        ] {
            app.editor.set_playhead(nle_timeline::Tick(logical_tick));
            let source = preview_request(&app.editor).sources[0].expect("VFR preview source");
            assert_eq!(source.source_tick, expected_decode_tick);
            assert_eq!(source.source_frame_rate, None);
            assert_eq!(source.source_frame_duration_tick, expected_duration);
        }
    }

    #[test]
    fn supplied_reordered_vfr_fixture_routes_preview_to_local_presentation_boundaries() {
        let Some(path) = std::env::var_os("MAELSTROM_REORDERED_VFR_TEST_MEDIA").map(PathBuf::from)
        else {
            return;
        };
        let metadata =
            nle_waveform::probe_media_metadata(&path).expect("probe reordered VFR fixture");
        let frame_timing =
            nle_waveform::analyze_frame_timing(&path).expect("scan reordered VFR fixture");
        let nle_waveform::FrameTiming::Variable(index) = &frame_timing else {
            panic!("reordered VFR fixture was not classified as variable");
        };
        assert_eq!(
            index.pts(),
            &[
                0, 41_666, 125_000, 166_666, 250_000, 333_333, 458_333, 500_000
            ]
        );

        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([path]);
        assert!(app.editor.add_selected_to_timeline());
        app.media_analysis_tx
            .send(MediaAnalysisResult {
                project_epoch: app.media_analysis_epoch,
                media_id: 1,
                is_still: false,
                metadata: Ok(metadata),
                frame_timing: Ok(frame_timing),
                waveform: Err("fixture intentionally has no audio".to_owned()),
                video_strip: Err("strip is irrelevant to timing proof".to_owned()),
            })
            .expect("queue reordered VFR analysis");
        app.poll_media_analysis();

        for (logical_tick, expected_decode_tick, expected_duration) in [
            (0, 0, Some(41_666)),
            (41_665, 0, Some(41_666)),
            (41_666, 41_666, Some(83_334)),
            (124_999, 41_666, Some(83_334)),
            (125_000, 125_000, Some(41_666)),
            (166_665, 125_000, Some(41_666)),
            (166_666, 166_666, Some(83_334)),
            (249_999, 166_666, Some(83_334)),
            (250_000, 250_000, Some(83_333)),
            (333_332, 250_000, Some(83_333)),
            (333_333, 333_333, Some(125_000)),
            (458_332, 333_333, Some(125_000)),
            (458_333, 458_333, Some(41_667)),
            (499_999, 458_333, Some(41_667)),
            (500_000, 500_000, None),
            (500_001, 500_000, None),
            (541_666, 500_000, None),
        ] {
            app.editor.set_playhead(nle_timeline::Tick(logical_tick));
            let source = preview_request(&app.editor).sources[0].expect("VFR preview source");
            assert_eq!(source.source_tick, expected_decode_tick);
            assert_eq!(source.source_frame_rate, None);
            assert_eq!(source.source_frame_duration_tick, expected_duration);
        }
    }

    #[test]
    fn supplied_shifted_10bit_vfr_fixtures_route_preview_to_local_boundaries() {
        for (variable, codec) in [
            ("MAELSTROM_PRORES_VFR_TEST_MEDIA", "prores"),
            ("MAELSTROM_DNXHR_VFR_TEST_MEDIA", "dnxhd"),
        ] {
            let Some(path) = std::env::var_os(variable).map(PathBuf::from) else {
                continue;
            };
            let metadata =
                nle_waveform::probe_media_metadata(&path).expect("probe shifted 10-bit fixture");
            assert_eq!(metadata.video_codec.as_deref(), Some(codec));
            let video = metadata
                .streams
                .iter()
                .find(|stream| stream.kind.as_deref() == Some("video"))
                .expect("fixture video stream");
            assert_eq!(video.start_seconds, Some(7.0));
            assert!((metadata.duration_seconds.unwrap() - 0.541_667).abs() < 0.000_001);
            let timing =
                nle_waveform::analyze_frame_timing(&path).expect("scan shifted 10-bit fixture");
            let nle_waveform::FrameTiming::Variable(index) = &timing else {
                panic!("{codec} fixture did not retain its irregular presentation timing");
            };
            let boundaries = [
                0, 41_667, 125_000, 166_667, 250_000, 333_333, 458_333, 500_000,
            ];
            assert_eq!(index.pts(), boundaries, "{codec} local presentation index");

            let mut app = App::new_with_catalog(false, None);
            app.editor.add_media_paths([path]);
            assert!(app.editor.add_selected_to_timeline());
            app.media_analysis_tx
                .send(MediaAnalysisResult {
                    project_epoch: app.media_analysis_epoch,
                    media_id: 1,
                    is_still: false,
                    metadata: Ok(metadata),
                    frame_timing: Ok(timing),
                    waveform: Err("fixture intentionally has no audio".into()),
                    video_strip: Err("strip is irrelevant to timing proof".into()),
                })
                .expect("queue shifted 10-bit analysis");
            app.poll_media_analysis();

            // Exercise both directions, each exact boundary, and the final microsecond
            // before the next boundary. The final frame has no invented CFR duration.
            for reverse in [false, true] {
                for step in 0..boundaries.len() {
                    let index = if reverse {
                        boundaries.len() - 1 - step
                    } else {
                        step
                    };
                    let boundary = boundaries[index];
                    let next = boundaries.get(index + 1).copied();
                    let duration = next.map(|next| next - boundary);
                    for logical_tick in [boundary, next.map_or(541_666, |next| next - 1)] {
                        app.editor.set_playhead(nle_timeline::Tick(logical_tick));
                        let source = preview_request(&app.editor).sources[0]
                            .expect("shifted 10-bit preview source");
                        assert_eq!(source.source_tick, boundary, "{codec} at {logical_tick}");
                        assert_eq!(source.source_frame_duration_tick, duration);
                        assert_eq!(source.source_frame_rate, None);
                    }
                }
            }
        }
    }

    #[test]
    fn scrub_submission_coalesces_to_source_frames_and_submits_matching_size_release() {
        let mut app = App::new_with_catalog(false, None);
        app.editor
            .add_media_paths([PathBuf::from("subframe-scrub.mp4")]);
        let video_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .expect("default video track")
            .id;
        app.editor
            .timeline
            .insert_clip(
                video_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        assert!(app.editor.set_preview_quality(PreviewQuality::Eighth));
        assert!(
            app.editor
                .set_paused_preview_quality(PreviewQuality::Eighth)
        );

        let frame_duration = app.editor.frame_duration_tick().0;
        let first = frame_duration.saturating_mul(10).saturating_add(1);
        let second = first.saturating_add(frame_duration / 2);
        let scrub_size = app.editor.monitor_scrub_decode_size_hint();
        let first_source_frame = monitor_source_tick_for_preview(first, None);
        let second_source_frame = monitor_source_tick_for_preview(second, None);

        app.editor.set_playhead(nle_timeline::Tick(first));
        let mut first_preview = preview_request(&app.editor);
        first_preview.is_scrubbing = true;
        first_preview.output_size = [scrub_size.0, scrub_size.1];
        app.submit_monitor_decode_request(first_preview);
        assert_eq!(
            app.monitor_last_requests[0]
                .expect("first scrub request")
                .source_tick,
            first_source_frame
        );

        app.editor.set_playhead(nle_timeline::Tick(second));
        let mut second_preview = preview_request(&app.editor);
        second_preview.is_scrubbing = true;
        second_preview.output_size = [scrub_size.0, scrub_size.1];
        app.submit_monitor_decode_request(second_preview);
        assert_eq!(
            app.monitor_last_requests[0]
                .expect("second scrub request")
                .source_tick,
            second_source_frame
        );

        let scrub_generation = app.monitor_generations[0];
        let scrub_request_id = app.monitor_latest_request_ids[0];
        assert!(
            app.monitor_last_requests[0]
                .expect("scrub request key")
                .is_scrubbing
        );
        let release_preview = preview_request(&app.editor);
        assert!(!release_preview.is_scrubbing);
        app.submit_monitor_decode_request(release_preview);
        assert_eq!(
            app.monitor_generations[0], scrub_generation,
            "equal Eighth-quality dimensions should retain the decoder session"
        );
        assert_ne!(app.monitor_latest_request_ids[0], scrub_request_id);
        assert!(
            !app.monitor_last_requests[0]
                .expect("release request key")
                .is_scrubbing,
            "release must submit even when its dimensions match scrub quality"
        );
        assert_eq!(
            app.monitor_last_requests[0]
                .expect("release refinement request")
                .source_tick,
            second_source_frame
        );
    }

    #[test]
    fn moving_and_scrub_preview_use_full_selected_resolution() {
        let mut editor = EditorState::new(Language::English, "Scrub preview size");

        assert_eq!(editor.preview_quality(), PreviewQuality::Full);
        assert!(editor.set_paused_preview_quality(PreviewQuality::Half));
        assert_eq!(preview_decode_size(&editor, false), (320, 180));
        assert_eq!(preview_decode_size(&editor, true), (640, 360));
        editor.playing = true;
        assert_eq!(preview_decode_size(&editor, false), (640, 360));
        editor.playing = false;

        assert!(editor.set_preview_quality(PreviewQuality::Eighth));
        assert_eq!(preview_decode_size(&editor, true), (80, 45));
    }

    #[test]
    fn every_scrub_publishes_timed_progressive_frames() {
        let mut editor = EditorState::new(Language::English, "Scrub policy");
        let mut preview = preview_request(&editor);
        assert!(!progressive_scrub_frames(&preview));
        preview.is_scrubbing = true;
        assert_eq!(preview.resolved_quality, PreviewQuality::Full);
        assert!(progressive_scrub_frames(&preview));

        assert!(editor.set_preview_quality(PreviewQuality::Quarter));
        let mut preview = preview_request(&editor);
        preview.is_scrubbing = true;
        assert!(progressive_scrub_frames(&preview));
    }

    #[test]
    fn full_quality_scrub_keeps_layer_resolution_and_submits_exact_release() {
        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([
            PathBuf::from("lower-scrub.mp4"),
            PathBuf::from("upper-scrub.mp4"),
        ]);
        let video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        for (track, media_id) in video_tracks.into_iter().zip(1..=2) {
            app.editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(15_000_000),
                    nle_timeline::Tick(0),
                )
                .unwrap();
        }
        assert_eq!(app.editor.preview_quality(), PreviewQuality::Full);
        app.editor.set_playhead(nle_timeline::Tick(7_500_000));

        let scrub_size = app.editor.monitor_playback_decode_size_hint();
        let mut scrub_preview = preview_request(&app.editor);
        scrub_preview.is_scrubbing = true;
        app.submit_monitor_decode_request(scrub_preview);
        let scrub_generations = app.monitor_generations;
        let scrub_request_ids = app.monitor_latest_request_ids;
        for layer in 0..2 {
            let request = app.monitor_last_requests[layer].expect("scrub layer request");
            assert_eq!((request.width, request.height), scrub_size);
            assert!(!request.prewarm_scrub_workers);
            app.editor.set_monitor_frame_for_layer(
                layer,
                egui::TextureId::Managed(70 + layer as u64),
                request.width,
                request.height,
                Some((layer + 1) as u32),
                Some(nle_timeline::Tick(request.source_tick)),
            );
        }

        let full_size = app.editor.monitor_playback_decode_size_hint();
        assert_eq!(full_size, scrub_size);
        let full_preview = preview_request(&app.editor);
        app.submit_monitor_decode_request(full_preview);
        for layer in 0..2 {
            let request = app.monitor_last_requests[layer].expect("refinement layer request");
            assert_eq!((request.width, request.height), full_size);
            assert_eq!(request.prewarm_scrub_workers, layer == 1);
            assert_eq!(app.monitor_generations[layer], scrub_generations[layer]);
            assert_eq!(
                app.editor
                    .monitor_frame_for_layer(layer)
                    .expect("last good scrub frame is retained")
                    .texture,
                egui::TextureId::Managed(70 + layer as u64),
            );
            assert!(!monitor_event_is_current(
                app.monitor_generations[layer],
                app.monitor_latest_request_ids[layer],
                scrub_generations[layer],
                scrub_request_ids[layer],
            ));
        }
    }

    #[test]
    fn preview_resolution_change_obsoletes_old_decode_but_holds_last_good_frame() {
        let mut app = App::new_with_catalog(false, None);
        app.editor
            .add_media_paths([PathBuf::from("preview-resolution-change.mp4")]);
        let video_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Video)
            .unwrap()
            .id;
        app.editor
            .timeline
            .insert_clip(
                video_track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        app.editor.set_playhead(nle_timeline::Tick(500_000));
        assert_eq!(app.editor.preview_quality(), PreviewQuality::Full);
        app.editor.playing = true;
        app.sync_monitor_decode();
        let full = app.monitor_last_requests[0].unwrap();
        app.editor.set_monitor_frame_for_layer(
            0,
            egui::TextureId::Managed(77),
            full.width,
            full.height,
            Some(1),
            Some(nle_timeline::Tick(full.source_tick)),
        );
        let old_generation = app.monitor_generations[0];

        assert!(app.editor.set_preview_quality(PreviewQuality::Half));
        app.sync_monitor_decode();

        let half = app.monitor_last_requests[0].unwrap();
        assert_ne!(app.monitor_generations[0], old_generation);
        assert_eq!((half.width, half.height), (full.width / 2, full.height / 2));
        assert_eq!(half.project_epoch, app.monitor_generations[0]);
        assert_eq!(
            app.editor
                .monitor_frame_for_layer(0)
                .map(|frame| frame.texture),
            Some(egui::TextureId::Managed(77))
        );
    }

    #[test]
    fn transformed_multilayer_preview_reaches_the_shared_composited_exporter() {
        let mut app = App::new_with_catalog(false, None);
        app.screen = Screen::Editor;
        app.editor.add_media_paths([
            PathBuf::from("transformed-export.mp4"),
            PathBuf::from("upper-export.mp4"),
        ]);
        let video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        let clip = app
            .editor
            .timeline
            .insert_clip(
                video_tracks[0],
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        let transform = nle_timeline::ClipTransform {
            rotation_degrees: 12.0,
            ..nle_timeline::ClipTransform::default()
        };
        app.editor
            .timeline
            .set_clip_transform(clip, transform)
            .unwrap();
        app.editor
            .timeline
            .insert_clip(
                video_tracks[1],
                nle_timeline::MediaId(2),
                nle_timeline::Tick(0),
                nle_timeline::Tick(2_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();

        assert_eq!(app.editor.quick_export_block_message(), None);
        let output = test_catalog_path("composited-transform-export").with_extension("mp4");
        app.start_video_export(output.clone());
        assert!(
            app.export_job.is_some(),
            "the composition passed both app gates"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.export_job.is_some() && Instant::now() < deadline {
            app.poll_video_export();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(app.export_job.is_none());
        assert!(matches!(
            app.editor.export_status,
            nle_ui_core::EditorExportStatus::Failed(_)
        ));
        assert!(!output.exists());
    }

    #[test]
    #[ignore = "requires four explicit dynamic 1920x1080 MPEG-4 fixtures and MAELSTROM_PHASE1_MULTISOURCE_REPORT"]
    fn supplied_media_four_video_layers_decode_independently() {
        let sources = phase1_multisource_sources().expect("validate four Phase 1 source fixtures");
        let report_path =
            phase1_multisource_report_path().expect("validate MAELSTROM_PHASE1_MULTISOURCE_REPORT");
        let mut app = App::new_with_catalog(false, None);
        app.editor
            .add_media_paths(sources.iter().map(|source| source.path.clone()));
        let mut video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while video_tracks.len() < MONITOR_LAYER_COUNT {
            video_tracks.push(
                app.editor
                    .timeline
                    .add_track(nle_timeline::TrackKind::Video),
            );
        }
        video_tracks.truncate(MONITOR_LAYER_COUNT);
        assert_eq!(video_tracks.len(), MONITOR_LAYER_COUNT);
        for (track, media_id) in video_tracks.into_iter().zip(1..=MONITOR_LAYER_COUNT as u32) {
            app.editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(2_000_000),
                    nle_timeline::Tick(0),
                )
                .unwrap();
        }
        const REQUESTED_SOURCE_TICK: i64 = 1_500_000;
        let expected_size = (1920, 1080);
        app.editor
            .set_playhead(nle_timeline::Tick(REQUESTED_SOURCE_TICK));
        app.editor.set_preview_quality(PreviewQuality::Full);
        app.editor.set_paused_preview_quality(PreviewQuality::Full);
        let mut preview = preview_request(&app.editor);
        assert_eq!(preview.output_size, [640, 360]);
        assert_eq!(preview.selected_quality, PreviewQuality::Full);
        assert_eq!(preview.resolved_quality, PreviewQuality::Full);
        assert!(
            preview.sources.iter().flatten().all(|source| {
                source.source_tick == REQUESTED_SOURCE_TICK && source.media_id > 0
            })
        );
        // This gate measures the app's immutable-request submission path at real 1080p output
        // dimensions. Decode remains asynchronous; no FFmpeg work is allowed inside this call.
        preview.output_size = [expected_size.0, expected_size.1];
        let submitted_at = Instant::now();
        app.submit_monitor_decode_request(preview);
        let submission_us = submitted_at.elapsed().as_micros();
        assert!(
            submission_us < 20_000,
            "paused four-source preview submission took {submission_us} us; expected less than 20000 us"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && (0..MONITOR_LAYER_COUNT)
                .any(|layer| app.editor.monitor_frame_for_layer(layer).is_none())
        {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(5));
        }
        let all_frames_ms = submitted_at.elapsed().as_millis();

        let decoded_frames = (0..MONITOR_LAYER_COUNT)
            .map(|layer| {
                app.editor
                    .monitor_frame_for_layer(layer)
                    .expect("validated monitor frame")
            })
            .collect::<Vec<_>>();
        let decoded_media = decoded_frames
            .iter()
            .map(|frame| frame.media_id)
            .collect::<Vec<_>>();
        assert_eq!(decoded_media, [Some(1), Some(2), Some(3), Some(4)]);
        for frame in &decoded_frames {
            assert_eq!((frame.width, frame.height), expected_size);
            assert!(
                frame
                    .source_tick
                    .is_some_and(|tick| tick.0 >= REQUESTED_SOURCE_TICK),
                "decoded source tick {:?} preceded requested mid-GOP tick {REQUESTED_SOURCE_TICK}",
                frame.source_tick
            );
        }
        assert!(app.monitor_latest_request_ids.iter().all(|id| *id > 0));
        assert!(app.monitor_requests_in_flight.iter().all(|active| !active));

        let pool_deadline = Instant::now() + Duration::from_secs(5);
        let pool_diagnostics = loop {
            let diagnostics = app.monitor_session_pool.diagnostics();
            if diagnostics.active_foreground_sessions == 4
                // The app-wide source coordinator deduplicates all three speculative lanes for
                // the top paused source into one background actor/session.
                && diagnostics.active_background_sessions == 1
                && diagnostics.peak_sticky_sessions == 5
                && diagnostics.session_cap == 8
            {
                break diagnostics;
            }
            assert!(
                Instant::now() < pool_deadline,
                "four-source paused prewarm did not establish the expected 4 foreground + 1 deduplicated background session pool: {diagnostics:?}"
            );
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(pool_diagnostics.active_sticky_sessions, 5);
        assert_eq!(pool_diagnostics.foreground_session_cap, 4);
        assert_eq!(pool_diagnostics.background_session_cap, 4);
        let source_diagnostics = app.monitor_source_coordinator.diagnostics();
        assert_eq!(source_diagnostics.live_source_groups, 4);
        assert_eq!(source_diagnostics.source_group_cap, 4);
        assert_eq!(source_diagnostics.live_lane_actors, 5);
        assert_eq!(source_diagnostics.lane_actor_cap, 8);
        assert_eq!(source_diagnostics.retiring_lane_actors, 0);
        assert!(
            !app.observed_decoder_backends.is_empty(),
            "final monitor frames did not report a decoder backend"
        );
        let observed_decoder_backends = app.observed_decoder_backends.clone();

        let monitor_session_pool = app.monitor_session_pool.clone();
        drop(app);
        let release_deadline = Instant::now() + Duration::from_secs(5);
        let post_drop_active_sessions = loop {
            let active = monitor_session_pool.diagnostics().active_sticky_sessions;
            if active == 0 {
                break active;
            }
            assert!(
                Instant::now() < release_deadline,
                "monitor decoder sessions remained active after App drop: {active}"
            );
            thread::sleep(Duration::from_millis(5));
        };

        let report = Phase1MultisourceReport {
            schema_version: 1,
            status: "passed",
            source_count: sources.len(),
            sources,
            decoded_media_ids: decoded_media
                .into_iter()
                .map(|media_id| media_id.expect("validated decoded media id"))
                .collect(),
            requested_source_tick: REQUESTED_SOURCE_TICK,
            decoded_source_ticks: decoded_frames
                .iter()
                .map(|frame| frame.source_tick.expect("validated decoded source tick").0)
                .collect(),
            observed_decoder_backends,
            output_size: [expected_size.0, expected_size.1],
            submission_us,
            all_frames_ms,
            active_sticky_sessions: pool_diagnostics.active_sticky_sessions,
            peak_sticky_sessions: pool_diagnostics.peak_sticky_sessions,
            session_cap: pool_diagnostics.session_cap,
            active_foreground_sessions: pool_diagnostics.active_foreground_sessions,
            foreground_session_cap: pool_diagnostics.foreground_session_cap,
            active_background_sessions: pool_diagnostics.active_background_sessions,
            background_session_cap: pool_diagnostics.background_session_cap,
            live_source_groups: source_diagnostics.live_source_groups,
            source_group_cap: source_diagnostics.source_group_cap,
            live_lane_actors: source_diagnostics.live_lane_actors,
            lane_actor_cap: source_diagnostics.lane_actor_cap,
            retiring_lane_actors: source_diagnostics.retiring_lane_actors,
            post_drop_active_sessions,
        };
        phase1_multisource_write_report(&report_path, &report)
            .expect("atomically write Phase 1 multisource report");
    }

    #[test]
    #[ignore = "requires four explicit dynamic 1920x1080 MPEG-4 fixtures and MAELSTROM_PHASE1_LATENCY_REPORT"]
    fn supplied_media_latency_comparison_uses_isolated_full_quality_trials() {
        const TRIALS_PER_SCENARIO: usize = 20;
        const INPUT_TO_SUBMIT_P95_US_LIMIT: u128 = 1_000;
        // Every value is deliberately inside a GOP, never on the one-second MPEG-4 keyframe.
        const MID_GOP_TICKS: [i64; 5] = [1_117_000, 1_283_000, 1_449_000, 1_617_000, 1_783_000];

        let sources = phase1_multisource_sources().expect("validate four Phase 1 source fixtures");
        let report_path =
            phase1_latency_report_path().expect("validate MAELSTROM_PHASE1_LATENCY_REPORT");
        let mut one_source_samples = Vec::with_capacity(TRIALS_PER_SCENARIO);
        let mut four_source_samples = Vec::with_capacity(TRIALS_PER_SCENARIO);
        let mut sequence_index = 0;
        for trial in 0..TRIALS_PER_SCENARIO {
            let tick = MID_GOP_TICKS[trial % MID_GOP_TICKS.len()];
            // Alternate which scenario receives the first cold start while retaining strict raw
            // sequence evidence in the report.
            for source_count in if trial % 2 == 0 { [1, 4] } else { [4, 1] } {
                let sample =
                    phase1_latency_trial(&sources, trial, sequence_index, source_count, tick);
                if source_count == 1 {
                    one_source_samples.push(sample);
                } else {
                    four_source_samples.push(sample);
                }
                sequence_index += 1;
            }
        }

        let one_source = phase1_latency_summary(&one_source_samples);
        let four_source = phase1_latency_summary(&four_source_samples);
        let passed = four_source.input_to_submit_us.p95 <= INPUT_TO_SUBMIT_P95_US_LIMIT;
        let comparison = Phase1LatencyComparison {
            input_to_submit_p95_delta_us: four_source.input_to_submit_us.p95 as i128
                - one_source.input_to_submit_us.p95 as i128,
            input_to_submit_p95_ratio: phase1_latency_ratio(
                four_source.input_to_submit_us.p95,
                one_source.input_to_submit_us.p95,
            ),
            frame_ready_p95_delta_ms: four_source.frame_ready_ms.p95 as i128
                - one_source.frame_ready_ms.p95 as i128,
            frame_ready_p95_ratio: phase1_latency_ratio(
                four_source.frame_ready_ms.p95,
                one_source.frame_ready_ms.p95,
            ),
        };
        let report = Phase1LatencyReport {
            schema_version: 1,
            status: if passed { "passed" } else { "failed" },
            trial_count_per_scenario: TRIALS_PER_SCENARIO,
            input_to_submit_p95_us_limit: INPUT_TO_SUBMIT_P95_US_LIMIT,
            sources,
            output_size: [1920, 1080],
            one_source,
            four_source,
            comparison,
        };
        phase1_multisource_write_report(&report_path, &report)
            .expect("atomically write Phase 1 latency report");
        assert!(
            passed,
            "four-source input-to-submit p95 was {} us; expected no more than {INPUT_TO_SUBMIT_P95_US_LIMIT} us; report written to {}",
            report.four_source.input_to_submit_us.p95,
            report_path.display(),
        );
    }

    #[test]
    #[ignore = "requires four explicit dynamic 1920x1080 MPEG-4 fixtures and MAELSTROM_PHASE1_SUSTAINED_REPORT"]
    fn supplied_media_four_video_layers_sustain_bounded_scrub_resources() {
        const INPUT_TO_SUBMIT_P95_US_LIMIT: u128 = 1_000;
        const CYCLE_TIMEOUT: Duration = Duration::from_secs(5);
        // Every position is inside a GOP. The order deliberately crosses forward and backward
        // seeks while remaining inside the five-second fixture loop.
        const MID_GOP_TICKS: [i64; 8] = [
            1_117_000, 2_283_000, 3_449_000, 1_617_000, 3_117_000, 1_283_000, 2_617_000, 1_783_000,
        ];

        let sources = phase1_multisource_sources().expect("validate four Phase 1 source fixtures");
        let report_path =
            phase1_sustained_report_path().expect("validate MAELSTROM_PHASE1_SUSTAINED_REPORT");
        let requested_duration_seconds = phase1_sustained_duration_seconds(
            std::env::var("MAELSTROM_PHASE1_SUSTAINED_SECONDS")
                .ok()
                .as_deref(),
        );
        let authoritative = requested_duration_seconds >= DEFAULT_PHASE1_SUSTAINED_SOAK_SECONDS;
        let mut app = phase1_multisource_app(&sources, MONITOR_LAYER_COUNT);
        app.editor.set_preview_quality(PreviewQuality::Full);
        app.editor.set_paused_preview_quality(PreviewQuality::Full);
        let baseline_diagnostics = RuntimeDiagnosticsReport::from(app.runtime_diagnostics());
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(requested_duration_seconds);
        let mut submissions_us = Vec::new();
        let mut frame_ready_ms = Vec::new();
        let mut source_exercise_counts = [0_u64; MONITOR_LAYER_COUNT];
        let mut max_decoded_tick_delta_us = 0_i64;
        let mut final_resources = aggregate_playback_soak_monitor_resources(
            &app.monitor_frame_cache_pool,
            app.monitor_session_pool.diagnostics(),
            app.monitor_source_coordinator.diagnostics(),
        );
        let mut cycles = 0_u64;

        while Instant::now() < deadline {
            let requested_source_tick = MID_GOP_TICKS[cycles as usize % MID_GOP_TICKS.len()];
            app.editor
                .set_playhead(nle_timeline::Tick(requested_source_tick));
            let mut preview = preview_request(&app.editor);
            assert_eq!(preview.selected_quality, PreviewQuality::Full);
            assert_eq!(preview.resolved_quality, PreviewQuality::Full);
            assert_eq!(preview.playhead_tick, requested_source_tick);
            assert!(
                preview
                    .sources
                    .iter()
                    .flatten()
                    .all(|source| source.media_id > 0)
            );
            preview.output_size = [1920, 1080];
            let submitted_at = Instant::now();
            app.submit_monitor_decode_request(preview);
            submissions_us.push(submitted_at.elapsed().as_micros());

            let cycle_deadline = Instant::now() + CYCLE_TIMEOUT;
            loop {
                app.poll_monitor_decoder();
                let matched = (0..MONITOR_LAYER_COUNT).all(|layer| {
                    app.editor
                        .monitor_frame_for_layer(layer)
                        .is_some_and(|frame| {
                            frame.media_id == Some(layer as u32 + 1)
                                && (frame.width, frame.height) == (1920, 1080)
                                && frame.source_tick.is_some_and(|tick| {
                                    tick.0 >= requested_source_tick
                                        && tick.0 <= requested_source_tick + 33_334
                                })
                                && !app.monitor_requests_in_flight[layer]
                        })
                });
                if matched {
                    break;
                }
                assert!(
                    Instant::now() < cycle_deadline,
                    "sustained cycle {cycles} did not receive four matching Full-output frames within {} ms",
                    CYCLE_TIMEOUT.as_millis(),
                );
                thread::sleep(Duration::from_millis(2));
            }
            frame_ready_ms.push(submitted_at.elapsed().as_millis());
            for layer in 0..MONITOR_LAYER_COUNT {
                let decoded_tick = app
                    .editor
                    .monitor_frame_for_layer(layer)
                    .and_then(|frame| frame.source_tick)
                    .expect("validated matching sustained source tick")
                    .0;
                max_decoded_tick_delta_us = max_decoded_tick_delta_us
                    .max(decoded_tick.saturating_sub(requested_source_tick));
            }
            for count in &mut source_exercise_counts {
                *count += 1;
            }
            let resources = aggregate_playback_soak_monitor_resources(
                &app.monitor_frame_cache_pool,
                app.monitor_session_pool.diagnostics(),
                app.monitor_source_coordinator.diagnostics(),
            );
            assert!(
                resources.active_sticky_sessions
                    == resources.active_foreground_sessions + resources.active_background_sessions
                    && resources.session_cap
                        == resources.foreground_session_cap + resources.background_session_cap
                    && resources.foreground_session_cap <= resources.session_cap
                    && resources.background_session_cap <= resources.session_cap
                    && resources.active_sticky_sessions <= resources.session_cap
                    && resources.peak_sticky_sessions <= resources.session_cap
                    && resources.active_foreground_sessions <= resources.foreground_session_cap
                    && resources.active_background_sessions <= resources.background_session_cap
                    && resources.live_source_groups <= resources.source_group_cap
                    && resources.live_lane_actors + resources.retiring_lane_actors
                        <= resources.lane_actor_cap,
                "sustained cycle {cycles} exceeded the shared session caps: {resources:?}"
            );
            assert!(
                resources.current_frame_cache_bytes <= resources.frame_cache_capacity_bytes
                    && resources.peak_frame_cache_bytes_upper_bound
                        <= resources.frame_cache_capacity_bytes,
                "sustained cycle {cycles} exceeded aggregate monitor cache capacity: {resources:?}"
            );
            final_resources = resources;
            cycles += 1;
        }

        let diagnostics_delta = RuntimeDiagnosticsReport::from(app.runtime_diagnostics())
            .delta_since(baseline_diagnostics);
        let expected_requests = cycles * MONITOR_LAYER_COUNT as u64;
        let monitor_dropped_frame_limit = phase1_sustained_dropped_frame_limit(expected_requests);
        let monitor_requests_complete = app.monitor_requests_in_flight.iter().all(|active| !active);
        let observed_backend = !app.observed_decoder_backends.is_empty();
        let observed_decoder_backends = app.observed_decoder_backends.clone();
        let monitor_session_pool = app.monitor_session_pool.clone();
        drop(app);
        let release_deadline = Instant::now() + CYCLE_TIMEOUT;
        let post_drop_active_sessions = loop {
            let active = monitor_session_pool.diagnostics().active_sticky_sessions;
            if active == 0 {
                break active;
            }
            assert!(
                Instant::now() < release_deadline,
                "monitor decoder sessions remained active after sustained soak App drop: {active}"
            );
            thread::sleep(Duration::from_millis(5));
        };

        let submission_distribution = phase1_latency_distribution(submissions_us.iter().copied());
        let frame_ready_distribution = phase1_latency_distribution(frame_ready_ms.iter().copied());
        let actual_duration_seconds = started_at.elapsed().as_secs_f64();
        let workload_valid = cycles >= MID_GOP_TICKS.len() as u64
            && source_exercise_counts.iter().all(|count| *count == cycles)
            && monitor_requests_complete
            && diagnostics_delta.monitor_requests == expected_requests
            && diagnostics_delta.monitor_completed_frames >= expected_requests
            && diagnostics_delta.monitor_presented_frames == diagnostics_delta.monitor_completed_frames
            // This counter is the scheduler's rejected obsolete/non-converging event count, not
            // displayed-frame loss. Bound it to 0.1% with a four-event startup allowance.
            && diagnostics_delta.monitor_dropped_frames <= monitor_dropped_frame_limit
            && diagnostics_delta.monitor_hold_events <= expected_requests
            && diagnostics_delta.monitor_late_frames <= expected_requests
            && diagnostics_delta.monitor_errors == 0
            && diagnostics_delta.native_viewer_uploads + diagnostics_delta.fallback_viewer_uploads
                == diagnostics_delta.monitor_presented_frames
            && diagnostics_delta.audio_underrun_frames == 0
            && diagnostics_delta.audio_callback_lock_failures == 0
            && diagnostics_delta.audio_late_discarded_frames == 0;
        let resources_valid = final_resources.current_frame_cache_bytes
            <= final_resources.frame_cache_capacity_bytes
            && final_resources.peak_frame_cache_bytes_upper_bound
                <= final_resources.frame_cache_capacity_bytes
            && final_resources.active_sticky_sessions
                == final_resources.active_foreground_sessions
                    + final_resources.active_background_sessions
            && final_resources.session_cap
                == final_resources.foreground_session_cap + final_resources.background_session_cap
            && final_resources.active_sticky_sessions <= final_resources.session_cap
            && final_resources.peak_sticky_sessions <= final_resources.session_cap
            && final_resources.active_foreground_sessions <= final_resources.foreground_session_cap
            && final_resources.active_background_sessions <= final_resources.background_session_cap
            && final_resources.live_source_groups <= final_resources.source_group_cap
            && final_resources.live_lane_actors + final_resources.retiring_lane_actors
                <= final_resources.lane_actor_cap;
        let passed = actual_duration_seconds >= requested_duration_seconds as f64
            && submission_distribution.p95 <= INPUT_TO_SUBMIT_P95_US_LIMIT
            && max_decoded_tick_delta_us <= 33_334
            && observed_backend
            && resources_valid
            && post_drop_active_sessions == 0
            && workload_valid;
        let report = Phase1SustainedReport {
            schema_version: 1,
            status: if passed { "passed" } else { "failed" },
            requested_duration_seconds,
            actual_duration_seconds,
            authoritative,
            source_count: sources.len(),
            sources,
            output_size: [1920, 1080],
            cycle_count: cycles,
            source_exercise_counts,
            requested_tick_pattern: MID_GOP_TICKS,
            max_decoded_tick_delta_us,
            monitor_dropped_frame_limit,
            input_to_submit_p95_us_limit: INPUT_TO_SUBMIT_P95_US_LIMIT,
            input_to_submit_samples_us: submissions_us,
            input_to_submit_us: submission_distribution,
            frame_ready_samples_ms: frame_ready_ms,
            frame_ready_ms: frame_ready_distribution,
            runtime_diagnostics_delta: diagnostics_delta,
            monitor_resources: final_resources,
            observed_decoder_backends,
            post_drop_active_sessions,
        };
        phase1_multisource_write_report(&report_path, &report)
            .expect("atomically write Phase 1 sustained soak report");
        assert!(
            passed,
            "four-source sustained gate failed: p95={} us (limit {INPUT_TO_SUBMIT_P95_US_LIMIT}), dropped={} (limit {}), tick_delta={} us, workload_valid={workload_valid}, resources_valid={resources_valid}, post_drop={post_drop_active_sessions}; report written to {}",
            report.input_to_submit_us.p95,
            report.runtime_diagnostics_delta.monitor_dropped_frames,
            report.monitor_dropped_frame_limit,
            report.max_decoded_tick_delta_us,
            report_path.display(),
        );
    }

    #[test]
    #[ignore = "requires four explicit dynamic 1920x1080 MPEG-4 fixtures and MAELSTROM_PHASE1_GENERATION_STRESS_REPORT"]
    fn supplied_media_layer_toggle_backward_scrub_stress() {
        const CYCLES: usize = 32;
        const CYCLE_TIMEOUT: Duration = Duration::from_secs(5);
        const OUTPUT_SIZE: [u32; 2] = [640, 360];
        const FORWARD_TICKS: [i64; 8] = [
            1_783_000, 2_117_000, 2_449_000, 2_783_000, 3_117_000, 3_449_000, 3_617_000, 3_783_000,
        ];

        let sources = phase1_multisource_sources().expect("validate four Phase 1 source fixtures");
        let report_path = phase1_generation_stress_report_path()
            .expect("validate MAELSTROM_PHASE1_GENERATION_STRESS_REPORT");
        let mut app = phase1_multisource_app(&sources, MONITOR_LAYER_COUNT);
        app.editor.set_preview_quality(PreviewQuality::Full);
        app.editor.set_paused_preview_quality(PreviewQuality::Full);
        let clips = (0..MONITOR_LAYER_COUNT)
            .map(|layer| {
                app.editor
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|track| &track.clips)
                    .find(|clip| clip.media == nle_timeline::MediaId(layer as u32 + 1))
                    .map(|clip| clip.id)
                    .expect("Phase 1 multisource app has one clip per fixture")
            })
            .collect::<Vec<_>>();
        let baseline_diagnostics = RuntimeDiagnosticsReport::from(app.runtime_diagnostics());

        // First retain real decoder pixels for all layers.  The direct receive below captures a
        // production DecodeEvent so the later stale proof does not rely on a synthetic frame.
        app.editor
            .set_playhead(nle_timeline::Tick(FORWARD_TICKS[0]));
        let mut initial = preview_request(&app.editor);
        initial.output_size = OUTPUT_SIZE;
        app.submit_monitor_decode_request(initial);
        let initial_deadline = Instant::now() + CYCLE_TIMEOUT;
        while Instant::now() < initial_deadline
            && (0..MONITOR_LAYER_COUNT).any(|layer| {
                app.editor.monitor_frame_for_layer(layer).is_none()
                    || app.monitor_requests_in_flight[layer]
            })
        {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(2));
        }
        assert!((0..MONITOR_LAYER_COUNT).all(|layer| {
            app.editor.monitor_frame_for_layer(layer).is_some()
                && !app.monitor_requests_in_flight[layer]
        }));

        app.editor
            .set_playhead(nle_timeline::Tick(FORWARD_TICKS[1]));
        let mut capture_preview = preview_request(&app.editor);
        capture_preview.output_size = OUTPUT_SIZE;
        app.submit_monitor_decode_request(capture_preview);
        let capture_deadline = Instant::now() + CYCLE_TIMEOUT;
        let captured_real_event = loop {
            match app.monitor_decoders[0]
                .try_recv()
                .expect("capture decoder open")
            {
                Some(event @ nle_decode::DecodeEvent::Frame(_)) => break event,
                Some(event) => {
                    let mut adaptive_quality_changed = false;
                    let _ = app.apply_monitor_decode_event(0, event, &mut adaptive_quality_changed);
                }
                None => {
                    assert!(
                        Instant::now() < capture_deadline,
                        "did not capture a real layer-zero decoder frame before generation stress"
                    );
                    // Do not call the general poller here: it would consume the exact real
                    // layer-zero event this test needs to capture and replay.
                    for layer in 1..MONITOR_LAYER_COUNT {
                        while let Some(event) = app.monitor_decoders[layer]
                            .try_recv()
                            .expect("non-captured decoder remains open")
                        {
                            let mut adaptive_quality_changed = false;
                            let _ = app.apply_monitor_decode_event(
                                layer,
                                event,
                                &mut adaptive_quality_changed,
                            );
                        }
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            }
        };
        let captured_real_identity = match &captured_real_event {
            nle_decode::DecodeEvent::Frame(frame) => (frame.project_epoch, frame.request_id),
            nle_decode::DecodeEvent::Error(_) => unreachable!("captured frame event"),
        };
        let captured_real_event_for_cycle = captured_real_event.clone();
        let mut adaptive_quality_changed = false;
        assert!(app.apply_monitor_decode_event(
            0,
            captured_real_event.clone(),
            &mut adaptive_quality_changed,
        ));

        // Advance layer zero's generation once before the loop and replay the real event. This
        // is deliberately identified in the report as a captured-event replay, not a live late
        // delivery; the barrier below separately proves an actual request was superseded.
        assert!(app.editor.set_timeline_clip_enabled(clips[0], false));
        let mut disabled_capture = preview_request(&app.editor);
        disabled_capture.output_size = OUTPUT_SIZE;
        app.submit_monitor_decode_request(disabled_capture);
        assert!(app.editor.monitor_frame_for_layer(0).is_none());
        assert!(app.editor.set_timeline_clip_enabled(clips[0], true));
        let mut restored_capture = preview_request(&app.editor);
        restored_capture.output_size = OUTPUT_SIZE;
        app.submit_monitor_decode_request(restored_capture);
        assert_eq!(app.editor.playback_targets().next().unwrap().media_id, 1);
        assert_ne!(app.monitor_generations[0], captured_real_identity.0);
        assert!(app.editor.monitor_frame_for_layer(0).is_none());
        assert!(app.monitor_last_proxy_frames[0].is_none());
        let captured_real_frame_rejected = !app.apply_monitor_decode_event(
            0,
            captured_real_event.clone(),
            &mut adaptive_quality_changed,
        );
        assert!(captured_real_frame_rejected);
        assert!(app.editor.monitor_frame_for_layer(0).is_none());
        // Hold pixels, source, request ID, and target constant: changing only the epoch must
        // make this otherwise eligible real event present. A media/convergence rejection alone
        // therefore cannot make the stale-generation proof pass.
        let nle_decode::DecodeEvent::Frame(mut epoch_control) = captured_real_event else {
            unreachable!("captured a real frame");
        };
        let control_generation = app.monitor_generations[0];
        epoch_control.project_epoch = control_generation;
        let matching_generation_control_presented = app.apply_monitor_decode_event(
            0,
            nle_decode::DecodeEvent::Frame(epoch_control),
            &mut adaptive_quality_changed,
        );
        assert!(matching_generation_control_presented);

        let mut per_cycle = Vec::with_capacity(CYCLES);
        let mut barrier_request_id = 0_u64;
        let mut barrier_blocked = false;
        let mut resource_checkpoint_count = 0_usize;
        let mut check_resources = |app: &App| {
            let resources = aggregate_playback_soak_monitor_resources(
                &app.monitor_frame_cache_pool,
                app.monitor_session_pool.diagnostics(),
                app.monitor_source_coordinator.diagnostics(),
            );
            assert!(
                phase1_live_audio_resources_are_bounded(&resources),
                "unbounded stress checkpoint: {resources:?}"
            );
            resource_checkpoint_count += 1;
        };
        for cycle in 0..CYCLES {
            // Start with the top positional source so the barrier's next request ID is exact;
            // later cycles rotate every independent fixture through the disable path.
            let toggled_layer = (cycle + MONITOR_LAYER_COUNT - 1) % MONITOR_LAYER_COUNT;
            let forward_playhead_tick = FORWARD_TICKS[cycle % FORWARD_TICKS.len()];
            let backward_playhead_tick = forward_playhead_tick - 166_667;
            app.editor
                .set_playhead(nle_timeline::Tick(forward_playhead_tick));
            let mut forward = preview_request(&app.editor);
            forward.output_size = OUTPUT_SIZE;

            // On the first cycle, block the exact real top-layer request before decoding, then
            // supersede it by disabling that layer. Guard Drop releases it during unwinding.
            let barrier = if cycle == 0 {
                barrier_request_id = app.monitor_next_request_id;
                Some(nle_decode::install_test_decode_barrier(
                    barrier_request_id,
                    sources[toggled_layer].path.clone(),
                ))
            } else {
                None
            };
            app.submit_monitor_decode_request(forward);
            if let Some(barrier) = &barrier {
                barrier.wait_until_blocked();
                barrier_blocked = barrier.is_blocked();
                assert!(barrier_blocked);
            }

            let forward_deadline = Instant::now() + CYCLE_TIMEOUT;
            while Instant::now() < forward_deadline
                && (0..MONITOR_LAYER_COUNT).any(|layer| {
                    layer != toggled_layer
                        && (app.editor.monitor_frame_for_layer(layer).is_none()
                            || app.monitor_requests_in_flight[layer])
                })
            {
                app.poll_monitor_decoder();
                thread::sleep(Duration::from_millis(2));
            }
            assert!((0..MONITOR_LAYER_COUNT).all(|layer| {
                layer == toggled_layer
                    || (app.editor.monitor_frame_for_layer(layer).is_some()
                        && !app.monitor_requests_in_flight[layer])
            }));
            let forward_identities = (0..MONITOR_LAYER_COUNT)
                .map(|layer| {
                    let request =
                        app.monitor_last_requests[layer].expect("forward retained request");
                    Phase1GenerationStressIdentity {
                        layer,
                        generation: app.monitor_generations[layer],
                        request_id: app.monitor_latest_request_ids[layer],
                        media_id: request.media_id,
                        source_tick: request.source_tick,
                    }
                })
                .collect();

            assert!(
                app.editor
                    .set_timeline_clip_enabled(clips[toggled_layer], false)
            );
            let mut disabled = preview_request(&app.editor);
            disabled.output_size = OUTPUT_SIZE;
            assert!(
                disabled
                    .sources
                    .iter()
                    .flatten()
                    .all(|source| source.media_id != toggled_layer as u32 + 1)
                    && disabled.sources[MONITOR_LAYER_COUNT - 1].is_none(),
                "disabled logical source must be absent after positional monitor-layer compaction"
            );
            app.submit_monitor_decode_request(disabled);
            check_resources(&app);
            let disabled_frame_cleared = !(0..MONITOR_LAYER_COUNT).any(|layer| {
                app.editor
                    .monitor_frame_for_layer(layer)
                    .is_some_and(|frame| frame.media_id == Some(toggled_layer as u32 + 1))
            });
            assert!(
                disabled_frame_cleared,
                "cycle {cycle} disabled layer retained pixels"
            );
            let disabled_deadline = Instant::now() + CYCLE_TIMEOUT;
            while Instant::now() < disabled_deadline
                && (0..MONITOR_LAYER_COUNT)
                    .filter(|layer| *layer != toggled_layer)
                    .any(|media_layer| {
                        !(0..MONITOR_LAYER_COUNT).any(|slot| {
                            app.editor
                                .monitor_frame_for_layer(slot)
                                .is_some_and(|frame| {
                                    frame.media_id == Some(media_layer as u32 + 1)
                                        && frame.source_tick.is_some_and(|tick| {
                                            tick.0 >= forward_playhead_tick
                                                && tick.0 <= forward_playhead_tick + 33_334
                                        })
                                })
                        })
                    })
            {
                app.poll_monitor_decoder();
                thread::sleep(Duration::from_millis(2));
            }
            // Disabling a lower timeline clip compacts positional monitor slots.  Assert retained
            // logical media identities after that remap, never stale positional texture slots.
            let unaffected_layers_retained = (0..MONITOR_LAYER_COUNT)
                .filter(|layer| *layer != toggled_layer)
                .all(|media_layer| {
                    (0..MONITOR_LAYER_COUNT).any(|slot| {
                        app.editor
                            .monitor_frame_for_layer(slot)
                            .is_some_and(|frame| {
                                frame.media_id == Some(media_layer as u32 + 1)
                                    && frame.source_tick.is_some_and(|tick| {
                                        tick.0 >= forward_playhead_tick
                                            && tick.0 <= forward_playhead_tick + 33_334
                                    })
                            })
                    })
                });
            assert!(
                unaffected_layers_retained,
                "cycle {cycle} did not retain unaffected logical media after slot remapping"
            );
            let disabled_identities = (0..MONITOR_LAYER_COUNT)
                .filter_map(|layer| {
                    app.monitor_last_requests[layer].map(|request| Phase1GenerationStressIdentity {
                        layer,
                        generation: app.monitor_generations[layer],
                        request_id: app.monitor_latest_request_ids[layer],
                        media_id: request.media_id,
                        source_tick: request.source_tick,
                    })
                })
                .collect();
            assert!(
                app.editor
                    .set_timeline_clip_enabled(clips[toggled_layer], true)
            );
            app.editor
                .set_playhead(nle_timeline::Tick(backward_playhead_tick));
            let mut backward = preview_request(&app.editor);
            backward.output_size = OUTPUT_SIZE;
            backward.is_scrubbing = true;
            app.submit_monitor_decode_request(backward);
            check_resources(&app);
            if let Some(barrier) = &barrier {
                barrier.release();
            }

            let settled_deadline = Instant::now() + CYCLE_TIMEOUT;
            let mut final_applied_identities = [None; MONITOR_LAYER_COUNT];
            while Instant::now() < settled_deadline
                && (0..MONITOR_LAYER_COUNT).any(|layer| {
                    !app.editor
                        .monitor_frame_for_layer(layer)
                        .is_some_and(|frame| {
                            frame.media_id == Some(layer as u32 + 1)
                                && frame.source_tick.is_some_and(|tick| {
                                    tick.0 >= backward_playhead_tick
                                        && tick.0 <= backward_playhead_tick + 33_334
                                })
                        })
                        || app.monitor_requests_in_flight[layer]
                })
            {
                // Keep the actual accepted DecodeEvent identity beside the final pixels.  The
                // normal production acceptance method remains the sole event application path.
                for (layer, applied_identity) in final_applied_identities.iter_mut().enumerate() {
                    while let Some(event) = app.monitor_decoders[layer]
                        .try_recv()
                        .expect("final decoder remains open")
                    {
                        let identity = match &event {
                            nle_decode::DecodeEvent::Frame(frame) => {
                                Some((frame.project_epoch, frame.request_id))
                            }
                            nle_decode::DecodeEvent::Error(_) => None,
                        };
                        if app.apply_monitor_decode_event(
                            layer,
                            event,
                            &mut adaptive_quality_changed,
                        ) {
                            assert_eq!(
                                identity.unwrap().0,
                                app.monitor_generations[layer],
                                "cycle {cycle} transiently presented an obsolete generation on layer {layer}"
                            );
                            *applied_identity = identity;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(
                (0..MONITOR_LAYER_COUNT).all(|layer| {
                    app.editor
                        .monitor_frame_for_layer(layer)
                        .is_some_and(|frame| {
                            frame.media_id == Some(layer as u32 + 1)
                                && (frame.width, frame.height) == (OUTPUT_SIZE[0], OUTPUT_SIZE[1])
                                && frame.source_tick.is_some_and(|tick| {
                                    tick.0 >= backward_playhead_tick
                                        && tick.0 <= backward_playhead_tick + 33_334
                                })
                        })
                        && !app.monitor_requests_in_flight[layer]
                }),
                "cycle {cycle} did not settle four current backward-scrub frames before deadline"
            );
            let latest_identities = (0..MONITOR_LAYER_COUNT)
                .map(|layer| {
                    let request = app.monitor_last_requests[layer].expect("final retained request");
                    let frame = app.editor.monitor_frame_for_layer(layer).expect("final retained frame");
                    assert_eq!(request.media_id, layer as u32 + 1);
                    assert_eq!(request.source_tick, backward_playhead_tick);
                    assert!(!app.monitor_requests_in_flight[layer]);
                    assert_eq!(
                        final_applied_identities[layer],
                        Some((app.monitor_generations[layer], app.monitor_latest_request_ids[layer])),
                        "cycle {cycle} final pixels were not accepted from the current generation/request"
                    );
                    Phase1GenerationStressIdentity {
                        layer,
                        generation: app.monitor_generations[layer],
                        request_id: app.monitor_latest_request_ids[layer],
                        media_id: frame.media_id.expect("final media id"),
                        source_tick: frame.source_tick.expect("final source tick").0,
                    }
                })
                .collect();
            let captured_real_frame_replay_rejected = !app.apply_monitor_decode_event(
                0,
                captured_real_event_for_cycle.clone(),
                &mut adaptive_quality_changed,
            );
            assert!(captured_real_frame_replay_rejected);
            check_resources(&app);
            per_cycle.push(Phase1GenerationStressCycle {
                cycle,
                toggled_layer,
                forward_playhead_tick,
                backward_playhead_tick,
                disabled_frame_cleared,
                unaffected_layers_retained,
                forward_identities,
                disabled_identities,
                latest_identities,
                final_applied_identities,
                captured_real_frame_replay_rejected,
            });
        }

        let diagnostics_delta = RuntimeDiagnosticsReport::from(app.runtime_diagnostics())
            .delta_since(baseline_diagnostics);
        let resources = aggregate_playback_soak_monitor_resources(
            &app.monitor_frame_cache_pool,
            app.monitor_session_pool.diagnostics(),
            app.monitor_source_coordinator.diagnostics(),
        );
        let resources_valid = phase1_live_audio_resources_are_bounded(&resources);
        assert!(
            resources_valid,
            "generation stress exceeded monitor resource bounds: {resources:?}"
        );
        assert_eq!(
            diagnostics_delta.monitor_errors, 0,
            "generation stress accepted a current decoder error"
        );
        assert!(
            !app.observed_decoder_backends.is_empty(),
            "real decoder backend was not observed"
        );
        let observed_decoder_backends = app.observed_decoder_backends.clone();
        let monitor_session_pool = app.monitor_session_pool.clone();
        let monitor_source_coordinator = app.monitor_source_coordinator.clone();
        drop(app);
        let release_deadline = Instant::now() + CYCLE_TIMEOUT;
        let post_drop = loop {
            let sessions = monitor_session_pool.diagnostics();
            let sources = monitor_source_coordinator.diagnostics();
            if sessions.active_sticky_sessions == 0
                && sources.live_source_groups == 0
                && sources.live_lane_actors + sources.retiring_lane_actors == 0
            {
                break Phase1GenerationStressPostDrop {
                    active_sessions: sessions.active_sticky_sessions,
                    live_source_groups: sources.live_source_groups,
                    live_lane_actors: sources.live_lane_actors,
                    retiring_lane_actors: sources.retiring_lane_actors,
                };
            }
            assert!(
                Instant::now() < release_deadline,
                "generation stress sources did not release after App drop: sessions={sessions:?} sources={sources:?}"
            );
            thread::sleep(Duration::from_millis(5));
        };

        let report = Phase1GenerationStressReport {
            schema_version: 1,
            status: "passed",
            source_count: sources.len(),
            cycles: CYCLES,
            sources,
            output_size: OUTPUT_SIZE,
            operations: Phase1GenerationStressOperations {
                forward_submits: CYCLES,
                backward_submits: CYCLES,
                disable_operations: CYCLES + 1,
                reenable_operations: CYCLES + 1,
                barrier_supersessions: usize::from(barrier_blocked),
            },
            observed_decoder_backends,
            stale_rejection: Phase1GenerationStressStaleRejection {
                barrier_blocked,
                barrier_request_id,
                captured_real_frame_identity: captured_real_identity,
                captured_real_frame_replayed_after_generation: true,
                captured_real_frame_rejected,
                matching_generation_control_presented,
                control_generation,
            },
            per_cycle,
            runtime_diagnostics_delta: diagnostics_delta,
            resources_valid,
            resource_checkpoint_count,
            resources,
            post_drop,
        };
        phase1_multisource_write_report(&report_path, &report)
            .expect("atomically write Phase 1 generation stress report");
    }

    #[test]
    #[ignore = "requires four dynamic 1920x1080 fixtures, a real default audio output, and MAELSTROM_PHASE1_LIVE_AUDIO_REPORT"]
    fn supplied_media_four_video_layers_preserve_live_audio_continuity() {
        const INPUT_TO_SUBMIT_P95_US_LIMIT: u128 = 1_000;
        const MAX_DEVICE_CLOCK_STALL: Duration = Duration::from_millis(250);
        const WARMUP_TIMEOUT: Duration = Duration::from_secs(10);
        const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
        const VIDEO_SAFE_SPAN_TICKS: i64 = 4_000_000;
        const VIDEO_SAFE_OFFSET_TICKS: i64 = 500_000;
        // A five-second default run still needs enough samples for nearest-rank p95 to tolerate
        // ordinary host preemption without masking a sustained scheduler regression.
        const MONITOR_SUBMIT_INTERVAL: Duration = Duration::from_millis(50);
        const CLOCK_DRIFT_LIMIT_US: i64 = 250_000;
        const MIN_MONITOR_REQUESTS_PER_SECOND: u64 = 8;
        const MIN_PRESENTATIONS_PER_SOURCE_PER_SECOND: u64 = 4;
        // Contributing layers are admitted topmost-first. Selecting the top layer makes the next
        // request ID deterministic and proves a delayed high-priority source cannot hide ready
        // lower layers.
        const SLOW_LAYER: usize = MONITOR_LAYER_COUNT - 1;
        const REQUESTED_BLOCKED_DURATION: Duration = Duration::from_millis(750);
        const MINIMUM_ACTUAL_BLOCKED_DURATION: Duration = REQUESTED_BLOCKED_DURATION;
        const MINIMUM_READY_SOURCE_PRESENTATIONS_DURING_BLOCK: u64 = 2;
        const MINIMUM_AUDIO_TICK_DELTA_DURING_BLOCK: i64 = 500_000;
        const MINIMUM_SLOW_SOURCE_PRESENTATIONS_AFTER_RELEASE: u64 = 1;

        let sources = phase1_multisource_sources().expect("validate four Phase 1 source fixtures");
        let audio_source = phase1_live_audio_source()
            .expect("validate MAELSTROM_PHASE1_AUDIO_MEDIA audio fixture");
        let report_path =
            phase1_live_audio_report_path().expect("validate MAELSTROM_PHASE1_LIVE_AUDIO_REPORT");
        let requested_duration_seconds = phase1_live_audio_duration_seconds(
            std::env::var("MAELSTROM_PHASE1_LIVE_AUDIO_SECONDS")
                .ok()
                .as_deref(),
        );
        let clip_duration_ticks = requested_duration_seconds
            .saturating_add(PHASE1_LIVE_AUDIO_WARMUP_RESERVE_SECONDS)
            .saturating_mul(1_000_000)
            .min(i64::MAX as u64) as i64;

        let mut app = phase1_multisource_app(&sources, MONITOR_LAYER_COUNT);
        app.editor.add_media_paths([audio_source.path.clone()]);
        let audio_media_id = app
            .editor
            .media
            .last()
            .filter(|media| media.path == audio_source.path)
            .map(|media| media.id)
            .expect("audio fixture was imported after the four video sources");
        let audio_track = app
            .editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == nle_timeline::TrackKind::Audio)
            .expect("default editor timeline has an audio track")
            .id;
        app.editor
            .timeline
            .insert_clip(
                audio_track,
                nle_timeline::MediaId(audio_media_id),
                nle_timeline::Tick(0),
                nle_timeline::Tick(clip_duration_ticks),
                nle_timeline::Tick(0),
            )
            .expect("insert live-audio gate clip");
        app.editor.set_preview_quality(PreviewQuality::Full);
        app.editor.set_paused_preview_quality(PreviewQuality::Full);
        assert_eq!(app.editor.audio_playback_targets().len(), 1);
        assert!(
            app.audio_engine_error.is_none(),
            "audio initialization failed: {:?}",
            app.audio_engine_error
        );
        assert!(
            app.audio_engine.is_some(),
            "real default audio output was not initialized"
        );

        app.editor.set_playhead(nle_timeline::Tick(0));
        app.editor.start_playback();
        app.sync_audio_transport();
        let callback_before_warmup = app
            .audio_engine
            .as_ref()
            .expect("validated audio engine")
            .runtime_diagnostics()
            .output_callback_cpu_timing
            .samples;
        let mix_before_warmup = app
            .audio_engine
            .as_ref()
            .expect("validated audio engine")
            .runtime_diagnostics()
            .mix_render_cpu_timing
            .samples;
        let warmup_deadline = Instant::now() + WARMUP_TIMEOUT;
        let (source_tick_start, warmup_max_meter) = loop {
            app.sync_audio_transport();
            let audio = app.audio_engine.as_ref().expect("validated audio engine");
            let diagnostics = audio.runtime_diagnostics();
            let (left, right) = audio.meter_levels();
            let meter = left.abs().max(right.abs());
            if let Some(source_tick) = audio.playback_source_tick()
                && app.audio_transport.is_some()
                && diagnostics.output_callback_cpu_timing.samples > callback_before_warmup
                && diagnostics.mix_render_cpu_timing.samples > mix_before_warmup
                && meter > 0.000_1
            {
                break (source_tick, meter);
            }
            assert!(
                app.audio_engine_error.is_none(),
                "audio reported an error during live-audio warmup: {:?}",
                app.audio_engine_error
            );
            assert!(
                Instant::now() < warmup_deadline,
                "live audio did not establish transport, advancing callback/mix samples, and a nonzero meter within {} ms",
                WARMUP_TIMEOUT.as_millis()
            );
            thread::sleep(Duration::from_millis(5));
        };

        let baseline_runtime_diagnostics =
            RuntimeDiagnosticsReport::from(app.runtime_diagnostics());
        let baseline_audio_diagnostics = app
            .audio_engine
            .as_ref()
            .expect("validated audio engine")
            .runtime_diagnostics();
        let measured_started_at = Instant::now();
        let measured_deadline =
            measured_started_at + Duration::from_secs(requested_duration_seconds);
        let mut next_monitor_submit = measured_started_at;
        let mut input_to_submit_us = Vec::new();
        let mut source_exercise_counts = [0_u64; MONITOR_LAYER_COUNT];
        let mut last_presented_source_ticks = [None; MONITOR_LAYER_COUNT];
        let mut max_device_clock_stall = Duration::ZERO;
        let mut last_clock_tick = source_tick_start;
        let mut last_clock_advance_at = measured_started_at;
        let mut max_meter = 0.0_f32;
        let mut final_meter = 0.0_f32;
        let mut meter_observation_count = 0_u64;
        let mut nonzero_meter_observation_count = 0_u64;
        let mut transport_lost = false;
        // The app allocates monitor request IDs in layer order. Install this before submitting
        // the next four-layer request so precisely the selected source stops at its worker edge.
        let mut slow_barrier = None;
        let mut slow_request_id = 0_u64;
        let mut slow_block_started_at = None;
        let mut slow_block_started_source_tick = None;
        let mut actual_blocked_duration = Duration::ZERO;
        let mut audio_tick_delta_during_block = 0_i64;
        let mut ready_source_presentations_during_block = 0_u64;
        let mut slow_source_presentations_after_release = 0_u64;
        let mut slow_barrier_released = false;

        while Instant::now() < measured_deadline {
            app.sync_audio_transport();
            let now = Instant::now();
            let audio = app.audio_engine.as_ref().expect("validated audio engine");
            let (left, right) = audio.meter_levels();
            final_meter = left.abs().max(right.abs());
            max_meter = max_meter.max(final_meter);
            meter_observation_count = meter_observation_count.saturating_add(1);
            if final_meter > 0.000_1 {
                nonzero_meter_observation_count = nonzero_meter_observation_count.saturating_add(1);
            }
            let source_tick = audio.playback_source_tick();
            transport_lost |= app.audio_transport.is_none() || app.audio_engine_error.is_some();
            if let Some(source_tick) = source_tick {
                if source_tick > last_clock_tick {
                    // Include the whole wall interval preceding this progress sample. Otherwise a
                    // long blocking decoder/poll call followed by one advancing device callback
                    // could reset the clock marker and hide the stall.
                    max_device_clock_stall =
                        max_device_clock_stall.max(now.duration_since(last_clock_advance_at));
                    last_clock_tick = source_tick;
                    last_clock_advance_at = now;
                } else {
                    max_device_clock_stall =
                        max_device_clock_stall.max(now.duration_since(last_clock_advance_at));
                }
                if now >= next_monitor_submit {
                    let video_tick = VIDEO_SAFE_OFFSET_TICKS
                        .saturating_add(source_tick.rem_euclid(VIDEO_SAFE_SPAN_TICKS));
                    let mut request = preview_request(&app.editor);
                    request.playhead_tick = video_tick;
                    request.output_size = [1920, 1080];
                    for source in request.sources.iter_mut().flatten() {
                        source.source_tick = video_tick;
                    }
                    assert_eq!(request.selected_quality, PreviewQuality::Full);
                    assert_eq!(request.resolved_quality, PreviewQuality::Full);
                    if slow_barrier.is_none() {
                        slow_request_id = app.monitor_next_request_id;
                        slow_barrier = Some(nle_decode::install_test_decode_barrier(
                            slow_request_id,
                            sources[SLOW_LAYER].path.clone(),
                        ));
                    }
                    let submitted_at = Instant::now();
                    app.submit_monitor_decode_request(request);
                    input_to_submit_us.push(submitted_at.elapsed().as_micros());
                    next_monitor_submit = now + MONITOR_SUBMIT_INTERVAL;
                }
            } else {
                transport_lost = true;
            }
            if let Some(barrier) = slow_barrier.as_ref()
                && !slow_barrier_released
                && barrier.is_blocked()
            {
                if slow_block_started_at.is_none() {
                    slow_block_started_at = Some(now);
                    slow_block_started_source_tick = source_tick;
                }
                if let Some(block_started_at) = slow_block_started_at
                    && now.duration_since(block_started_at) >= REQUESTED_BLOCKED_DURATION
                {
                    actual_blocked_duration = now.duration_since(block_started_at);
                    audio_tick_delta_during_block = source_tick
                        .unwrap_or(last_clock_tick)
                        .saturating_sub(slow_block_started_source_tick.unwrap_or(last_clock_tick));
                    barrier.release();
                    slow_barrier_released = true;
                }
            }
            app.poll_monitor_decoder();
            for layer in 0..MONITOR_LAYER_COUNT {
                if let Some(frame) = app.editor.monitor_frame_for_layer(layer)
                    && frame.media_id == Some(layer as u32 + 1)
                    && (frame.width, frame.height) == (1920, 1080)
                    && last_presented_source_ticks[layer] != frame.source_tick.map(|tick| tick.0)
                {
                    last_presented_source_ticks[layer] = frame.source_tick.map(|tick| tick.0);
                    source_exercise_counts[layer] += 1;
                    if slow_block_started_at.is_some()
                        && !slow_barrier_released
                        && layer != SLOW_LAYER
                    {
                        ready_source_presentations_during_block =
                            ready_source_presentations_during_block.saturating_add(1);
                    }
                    if slow_barrier_released && layer == SLOW_LAYER {
                        slow_source_presentations_after_release =
                            slow_source_presentations_after_release.saturating_add(1);
                    }
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        // A failed or shortened measurement must never leave a worker blocked while App drops
        // and joins its decoder sessions. Keep `slow_barrier_released` false here so the report
        // still records that the requested completed block/recovery was not observed.
        if let Some(barrier) = slow_barrier.as_ref()
            && !slow_barrier_released
        {
            barrier.release();
        }
        max_device_clock_stall =
            max_device_clock_stall.max(Instant::now().duration_since(last_clock_advance_at));
        // End while the four-source workload is still live. Requiring the final supersedable
        // monitor request to settle would turn normal end-of-load cancellation into an audio
        // continuity failure and extend the measured device interval by the teardown timeout.
        app.sync_audio_transport();
        app.poll_monitor_decoder();
        let actual_duration_seconds = measured_started_at.elapsed().as_secs_f64();
        let final_resources = aggregate_playback_soak_monitor_resources(
            &app.monitor_frame_cache_pool,
            app.monitor_session_pool.diagnostics(),
            app.monitor_source_coordinator.diagnostics(),
        );
        let audio = app.audio_engine.as_ref().expect("validated audio engine");
        let final_audio_diagnostics = audio.runtime_diagnostics();
        let source_tick_end = audio.playback_source_tick().unwrap_or(last_clock_tick);
        let source_tick_delta = source_tick_end.saturating_sub(source_tick_start);
        let expected_source_tick_delta = (actual_duration_seconds * 1_000_000.0).round() as i64;
        let clock_drift_us = source_tick_delta
            .saturating_sub(expected_source_tick_delta)
            .abs();
        let runtime_diagnostics_delta = RuntimeDiagnosticsReport::from(app.runtime_diagnostics())
            .delta_since(baseline_runtime_diagnostics);
        let callback_sample_delta = final_audio_diagnostics
            .output_callback_cpu_timing
            .samples
            .saturating_sub(
                baseline_audio_diagnostics
                    .output_callback_cpu_timing
                    .samples,
            );
        let mix_sample_delta = final_audio_diagnostics
            .mix_render_cpu_timing
            .samples
            .saturating_sub(baseline_audio_diagnostics.mix_render_cpu_timing.samples);
        let audio_counter_delta = Phase1LiveAudioCounterDelta {
            callback_lock_failures: final_audio_diagnostics
                .callback_lock_failures
                .saturating_sub(baseline_audio_diagnostics.callback_lock_failures),
            underrun_device_frames: final_audio_diagnostics
                .underrun_device_frames
                .saturating_sub(baseline_audio_diagnostics.underrun_device_frames),
            late_decoded_frames_discarded: final_audio_diagnostics
                .late_decoded_frames_discarded
                .saturating_sub(baseline_audio_diagnostics.late_decoded_frames_discarded),
        };
        let observed_decoder_backends = app.observed_decoder_backends.clone();
        let submission_distribution =
            phase1_latency_distribution(input_to_submit_us.iter().copied());
        let monitor_request_count = input_to_submit_us.len() as u64;
        let minimum_monitor_request_count =
            requested_duration_seconds.saturating_mul(MIN_MONITOR_REQUESTS_PER_SECOND);
        let minimum_presentations_per_source =
            requested_duration_seconds.saturating_mul(MIN_PRESENTATIONS_PER_SOURCE_PER_SECOND);
        let submitted_monitor_layer_requests =
            monitor_request_count.saturating_mul(MONITOR_LAYER_COUNT as u64);
        let minimum_monitor_layer_requests =
            minimum_monitor_request_count.saturating_mul(MONITOR_LAYER_COUNT as u64);
        let minimum_nonzero_meter_observations =
            meter_observation_count.saturating_mul(9).div_ceil(10);
        let resources_bounded = phase1_live_audio_resources_are_bounded(&final_resources);
        let audio_error = app.audio_engine_error.clone();
        let slow_source_recovered = slow_source_presentations_after_release
            >= MINIMUM_SLOW_SOURCE_PRESENTATIONS_AFTER_RELEASE;
        let passed_before_drop = actual_duration_seconds >= requested_duration_seconds as f64
            && monitor_request_count >= minimum_monitor_request_count
            && source_exercise_counts
                .iter()
                .all(|count| *count >= minimum_presentations_per_source)
            && source_tick_delta > 0
            && clock_drift_us <= CLOCK_DRIFT_LIMIT_US
            && callback_sample_delta > 0
            && mix_sample_delta > 0
            && max_meter > 0.000_1
            && final_meter > 0.000_1
            && meter_observation_count > 0
            && nonzero_meter_observation_count >= minimum_nonzero_meter_observations
            && max_device_clock_stall <= MAX_DEVICE_CLOCK_STALL
            && submission_distribution.p95 <= INPUT_TO_SUBMIT_P95_US_LIMIT
            && !transport_lost
            && audio_error.is_none()
            && audio_counter_delta.callback_lock_failures == 0
            && audio_counter_delta.underrun_device_frames == 0
            && audio_counter_delta.late_decoded_frames_discarded == 0
            && runtime_diagnostics_delta.monitor_requests >= minimum_monitor_layer_requests
            && runtime_diagnostics_delta.monitor_requests <= submitted_monitor_layer_requests
            && runtime_diagnostics_delta
                .monitor_requests
                .is_multiple_of(MONITOR_LAYER_COUNT as u64)
            && runtime_diagnostics_delta.monitor_completed_frames
                >= minimum_presentations_per_source.saturating_mul(MONITOR_LAYER_COUNT as u64)
            && runtime_diagnostics_delta.monitor_presented_frames
                >= minimum_presentations_per_source.saturating_mul(MONITOR_LAYER_COUNT as u64)
            && runtime_diagnostics_delta.monitor_errors == 0
            && !observed_decoder_backends.is_empty()
            && resources_bounded
            && slow_request_id != 0
            && slow_block_started_at.is_some()
            && slow_barrier_released
            && actual_blocked_duration >= MINIMUM_ACTUAL_BLOCKED_DURATION
            && ready_source_presentations_during_block
                >= MINIMUM_READY_SOURCE_PRESENTATIONS_DURING_BLOCK
            && audio_tick_delta_during_block >= MINIMUM_AUDIO_TICK_DELTA_DURING_BLOCK
            && slow_source_recovered;

        // This is deliberately a pause, not an editor/playhead reset: the release check must not
        // disguise a transport discontinuity by seeking before decoder workers are torn down.
        app.editor.playing = false;
        app.sync_audio_transport();
        let monitor_session_pool = app.monitor_session_pool.clone();
        drop(app);
        let release_deadline = Instant::now() + RELEASE_TIMEOUT;
        let post_drop_active_sessions = loop {
            let active = monitor_session_pool.diagnostics().active_sticky_sessions;
            if active == 0 {
                break active;
            }
            assert!(
                Instant::now() < release_deadline,
                "monitor decoder sessions remained active after live-audio App drop: {active}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let passed = passed_before_drop && post_drop_active_sessions == 0;
        let report = Phase1LiveAudioReport {
            schema_version: 2,
            status: if passed { "passed" } else { "failed" },
            requested_duration_seconds,
            actual_duration_seconds,
            source_count: sources.len(),
            video_sources: sources,
            audio_source,
            clip_duration_ticks,
            audio_target_count: 1,
            source_exercise_counts,
            slow_layer: SLOW_LAYER,
            slow_request_id,
            requested_blocked_duration_ms: REQUESTED_BLOCKED_DURATION.as_millis(),
            actual_blocked_duration_ms: actual_blocked_duration.as_millis(),
            minimum_actual_blocked_duration_ms: MINIMUM_ACTUAL_BLOCKED_DURATION.as_millis(),
            ready_source_presentations_during_block,
            minimum_ready_source_presentations_during_block:
                MINIMUM_READY_SOURCE_PRESENTATIONS_DURING_BLOCK,
            audio_tick_delta_during_block,
            minimum_audio_tick_delta_during_block: MINIMUM_AUDIO_TICK_DELTA_DURING_BLOCK,
            slow_source_presentations_after_release,
            minimum_slow_source_presentations_after_release:
                MINIMUM_SLOW_SOURCE_PRESENTATIONS_AFTER_RELEASE,
            slow_source_recovered,
            source_tick_start,
            source_tick_end,
            source_tick_delta,
            expected_source_tick_delta,
            clock_drift_us,
            clock_drift_limit_us: CLOCK_DRIFT_LIMIT_US,
            callback_sample_delta,
            mix_sample_delta,
            max_device_clock_stall_ms: max_device_clock_stall.as_millis(),
            max_device_clock_stall_limit_ms: MAX_DEVICE_CLOCK_STALL.as_millis(),
            warmup_max_meter,
            max_meter,
            final_meter,
            meter_observation_count,
            nonzero_meter_observation_count,
            minimum_nonzero_meter_observations,
            monitor_request_count,
            minimum_monitor_request_count,
            minimum_presentations_per_source,
            input_to_submit_p95_us_limit: INPUT_TO_SUBMIT_P95_US_LIMIT,
            input_to_submit_samples_us: input_to_submit_us,
            input_to_submit_us: submission_distribution,
            runtime_diagnostics_delta,
            audio_counter_delta,
            transport_lost,
            audio_error,
            monitor_resources: final_resources,
            observed_decoder_backends,
            post_drop_active_sessions,
        };
        phase1_multisource_write_report(&report_path, &report)
            .expect("atomically write Phase 1 live-audio report");
        assert!(
            passed,
            "four-source live-audio gate failed: duration={:.3}s sources={:?} slow_layer={} slow_request={} blocked={}ms ready_during_block={} audio_tick_delta_during_block={} recovered={} tick_delta={} callback_delta={} mix_delta={} meter={} stall={}ms p95={}us transport_lost={} audio_error={:?} audio_delta={:?} resources={:?} post_drop={}; report written to {}",
            report.actual_duration_seconds,
            report.source_exercise_counts,
            report.slow_layer,
            report.slow_request_id,
            report.actual_blocked_duration_ms,
            report.ready_source_presentations_during_block,
            report.audio_tick_delta_during_block,
            report.slow_source_recovered,
            report
                .source_tick_end
                .saturating_sub(report.source_tick_start),
            report.callback_sample_delta,
            report.mix_sample_delta,
            report.max_meter,
            report.max_device_clock_stall_ms,
            report.input_to_submit_us.p95,
            report.transport_lost,
            report.audio_error,
            report.audio_counter_delta,
            report.monitor_resources,
            report.post_drop_active_sessions,
            report_path.display(),
        );
    }

    #[test]
    fn supplied_media_missing_upper_layer_does_not_block_ready_lower_layer() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let missing = test_catalog_path("missing-upper-video").with_extension("mp4");
        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths([PathBuf::from(path), missing]);
        let video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .take(MONITOR_LAYER_COUNT)
            .collect::<Vec<_>>();
        for (track, media_id) in video_tracks.into_iter().zip(1..=MONITOR_LAYER_COUNT as u32) {
            app.editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(2_000_000),
                    nle_timeline::Tick(0),
                )
                .unwrap();
        }
        app.editor.set_playhead(nle_timeline::Tick(500_000));
        app.sync_monitor_decode();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && app.editor.monitor_frame_for_layer(0).is_none() {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(
            app.editor
                .monitor_frame_for_layer(0)
                .and_then(|frame| frame.media_id),
            Some(1)
        );
        assert!(app.editor.monitor_frame_for_layer(1).is_none());
    }

    #[test]
    fn corrupt_catalog_falls_back_to_backup() {
        let path = test_catalog_path("backup");
        fs::create_dir_all(path.parent().expect("test catalog parent")).expect("create test data");
        fs::write(&path, b"not json").expect("write corrupt primary");
        let projects = vec![project(3, "Recovered Cut", "Yesterday")];
        let backup = catalog_backup_path(&path);
        let catalog = ProjectCatalog {
            version: PROJECT_CATALOG_VERSION,
            projects: projects.iter().map(Into::into).collect(),
        };
        fs::write(
            backup,
            serde_json::to_vec(&catalog).expect("serialize backup"),
        )
        .expect("write backup");
        assert_eq!(load_catalog(&path), projects);
        fs::remove_dir_all(path.parent().expect("test catalog parent")).expect("remove test data");
    }

    #[test]
    fn startup_repairs_stale_catalog_size_from_project_document() {
        let catalog = test_catalog_path("repair-size");
        let project_path = project_document_path(&catalog, 3);
        let projects = vec![project(3, "Sized Cut", "Yesterday")];
        persist_catalog(&catalog, &projects).expect("write stale catalog");
        persist_project_document(&SaveRequest {
            project_path: project_path.clone(),
            document: test_document(
                &project_path,
                EditorState::new(Language::English, "Sized Cut").snapshot(),
            ),
            thumbnail: None,
        })
        .expect("write project document");

        let expected = format_file_size(
            fs::metadata(&project_path)
                .expect("saved project metadata")
                .len(),
        );
        let resources = load_startup_resources(Some(catalog.clone()));
        let loaded = resources.catalog.expect("startup catalog").0;
        assert_eq!(loaded[0].size, expected);
        assert_ne!(loaded[0].size, "0 B");

        fs::remove_dir_all(catalog.parent().expect("test catalog parent"))
            .expect("remove test data");
    }

    #[test]
    fn new_and_open_actions_persist_catalog_without_demo_writes() {
        let path = test_catalog_path("actions");
        let mut app = App::new_with_catalog(false, Some(path.clone()));
        app.handle_hub_action(HubAction::NewProject {
            name: "Session One".to_owned(),
            template: nle_ui_core::TemplateId::FullHd1080p,
            language: Language::English,
        });
        app.catalog_writer.flush();
        assert_eq!(app.hub.selected, Some(1));
        assert_eq!(load_catalog(&path)[0].name, "Session One");
        assert_eq!(app.current_project_settings.size, [1920, 1080]);
        assert_eq!(app.current_project_settings.fps, [30, 1]);
        assert_eq!(app.editor.project_canvas_size(), (1920, 1080));
        app.flush_project_autosave();
        let saved = load_project_document(&project_document_path(&path, 1))
            .expect("load new project")
            .expect("new project document");
        assert_eq!(saved.project_name, "Session One");
        assert_eq!(saved.size, [1920, 1080]);
        app.catalog_writer.flush();
        let expected_size = format_file_size(
            fs::metadata(project_document_path(&path, 1))
                .expect("saved project metadata")
                .len(),
        );
        assert_eq!(app.hub.projects[0].size, expected_size);
        assert_eq!(load_catalog(&path)[0].size, expected_size);
        app.hub.projects[0].recent = "Yesterday".to_owned();
        app.queue_project_catalog_save();
        app.catalog_writer.flush();
        app.handle_hub_action(HubAction::OpenExisting {
            project_id: 1,
            language: Language::English,
        });
        wait_for_project_open(&mut app);
        app.catalog_writer.flush();
        assert_eq!(load_catalog(&path)[0].recent, "Just now");
        assert_eq!(load_catalog(&path)[0].size, expected_size);
        drop(app);
        fs::remove_dir_all(path.parent().expect("test catalog parent")).expect("remove test data");
    }

    #[test]
    fn closing_and_reopening_project_restores_media_and_linked_clips() {
        let path = test_catalog_path("reopen-editor");
        {
            let mut app = App::new_with_catalog(false, Some(path.clone()));
            app.handle_hub_action(HubAction::NewProject {
                name: "Persistent Timeline".to_owned(),
                template: nle_ui_core::TemplateId::FullHd1080p,
                language: Language::English,
            });
            app.editor
                .add_media_paths([PathBuf::from("persistent-video.mp4")]);
            assert!(app.editor.add_selected_to_timeline());
            app.flush_project_autosave();
        }

        let mut reopened = App::new_with_catalog(false, Some(path.clone()));
        reopened.handle_hub_action(HubAction::OpenExisting {
            project_id: 1,
            language: Language::English,
        });
        wait_for_project_open(&mut reopened);
        assert_eq!(reopened.editor.media.len(), 1);
        assert_eq!(
            reopened
                .editor
                .timeline
                .tracks
                .iter()
                .map(|track| track.clips.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(reopened.editor.timeline.tracks[0].clips[0].start.0, 0);
        drop(reopened);
        fs::remove_dir_all(path.parent().expect("test catalog parent")).expect("remove test data");
    }

    #[test]
    fn analyzed_duration_autosaves_without_a_video_strip_and_survives_reopen() {
        let path = test_catalog_path("reopen-analyzed-duration");
        {
            let mut app = App::new_with_catalog(false, Some(path.clone()));
            app.handle_hub_action(HubAction::NewProject {
                name: "Analyzed Timeline".to_owned(),
                template: nle_ui_core::TemplateId::FullHd1080p,
                language: Language::English,
            });
            app.project_writer.flush();
            app.editor
                .add_media_paths([PathBuf::from("sixty-seconds.mp4")]);
            assert!(app.editor.add_selected_to_timeline());
            assert_eq!(app.editor.timeline_end(), nle_timeline::Tick(15_000_000));

            app.media_analysis_tx
                .send(MediaAnalysisResult {
                    project_epoch: app.media_analysis_epoch,
                    media_id: 1,
                    is_still: false,
                    metadata: Ok(nle_waveform::MediaMetadata {
                        duration_seconds: Some(60.0),
                        ..Default::default()
                    }),
                    frame_timing: Ok(nle_waveform::FrameTiming::Unknown),
                    waveform: Ok(nle_waveform::Waveform {
                        peaks: vec![
                            nle_waveform::Peak {
                                min: -0.25,
                                max: 0.25,
                            };
                            32
                        ],
                        sample_rate: Some(48_000),
                        channels: Some(2),
                        total_frames: 2_880_000,
                        duration_seconds: Some(60.0),
                    }),
                    video_strip: Err("thumbnail unavailable".to_owned()),
                })
                .expect("queue analyzed media result");
            app.poll_media_analysis();

            assert_eq!(
                app.editor.media[0].duration,
                Some(nle_timeline::Tick(60_000_000))
            );
            assert_eq!(app.editor.timeline_end(), nle_timeline::Tick(60_000_000));
            let deadline = app
                .autosave_schedule
                .deadline()
                .expect("analysis schedules autosave without a video strip");
            app.queue_project_autosave_at(deadline, false);
            app.project_writer.flush();
        }

        let mut reopened = App::new_with_catalog(false, Some(path.clone()));
        reopened.handle_hub_action(HubAction::OpenExisting {
            project_id: 1,
            language: Language::English,
        });
        wait_for_project_open(&mut reopened);
        assert_eq!(
            reopened.editor.media[0].duration,
            Some(nle_timeline::Tick(60_000_000))
        );
        assert_eq!(
            reopened.editor.timeline_end(),
            nle_timeline::Tick(60_000_000)
        );
        assert!(reopened.editor.add_selected_to_timeline());
        assert_eq!(
            reopened.editor.timeline_end(),
            nle_timeline::Tick(120_000_000),
            "future placement must use the duration restored from disk"
        );
        drop(reopened);
        fs::remove_dir_all(path.parent().expect("test catalog parent")).expect("remove test data");
    }

    #[test]
    fn external_project_registration_persists_path_and_analyzes_used_media_only() {
        let catalog = test_catalog_path("external-project");
        let project_path = catalog
            .parent()
            .expect("catalog parent")
            .join("portable/Outside.nleproj");
        let mut source = EditorState::new(Language::English, "Outside");
        source.add_media_paths([PathBuf::from("used.mp4"), PathBuf::from("unused.mp4")]);
        source.selected_media = Some(1);
        assert!(source.add_selected_to_timeline());
        let document = nle_project_io::document_for_path(
            &project_path,
            "Outside",
            source.snapshot(),
            ProjectSettings {
                fps: [24, 1],
                size: [3840, 2160],
            },
        );
        nle_project_io::write_document(&project_path, &document).expect("write portable project");

        let mut app = App::new_with_catalog(false, Some(catalog.clone()));
        app.complete_project_open(
            None,
            project_path.clone(),
            Language::English,
            fs::metadata(&project_path)
                .ok()
                .map(|metadata| metadata.len()),
            document,
        );

        assert_eq!(app.current_project_settings.size, [3840, 2160]);
        assert_eq!(app.editor.project_canvas_size(), (3840, 2160));
        assert_eq!(app.editor.frame_rate.numerator(), 24);
        assert_eq!(app.editor.frame_rate.denominator(), 1);
        assert_eq!(app.project_paths.get(&1), Some(&project_path));
        let queued_ids = app
            .media_analysis_pending
            .iter()
            .map(|(_, media_id, _)| *media_id)
            .chain(
                app.media_analysis_in_flight
                    .iter()
                    .map(|(_, media_id)| *media_id),
            )
            .collect::<HashSet<_>>();
        assert_eq!(queued_ids, HashSet::from([1]));
        app.catalog_writer.flush();
        let (_, paths) = load_catalog_with_paths(&catalog);
        assert_eq!(paths.get(&1), Some(&project_path));
        drop(app);
        fs::remove_dir_all(catalog.parent().expect("test root")).expect("remove test data");
    }

    #[derive(Serialize)]
    struct Phase0ScenarioReport {
        name: &'static str,
        iterations: u32,
        elapsed_ms: u128,
        decoder_backend: Option<String>,
        encoder_backend: Option<String>,
        passed: bool,
        evidence: String,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Phase0BackendRole {
        Decoder,
        Encoder,
    }

    #[derive(Serialize)]
    struct Phase0Report {
        schema_version: u32,
        status: &'static str,
        scenario_count: usize,
        scenarios: Vec<Phase0ScenarioReport>,
    }

    #[derive(Clone, Serialize)]
    struct Phase1MultisourceSource {
        path: PathBuf,
        size_bytes: u64,
    }

    #[derive(Serialize)]
    struct Phase1MultisourceReport {
        schema_version: u32,
        status: &'static str,
        source_count: usize,
        sources: Vec<Phase1MultisourceSource>,
        decoded_media_ids: Vec<u32>,
        requested_source_tick: i64,
        decoded_source_ticks: Vec<i64>,
        observed_decoder_backends: Vec<String>,
        output_size: [u32; 2],
        submission_us: u128,
        all_frames_ms: u128,
        active_sticky_sessions: usize,
        peak_sticky_sessions: usize,
        session_cap: usize,
        active_foreground_sessions: usize,
        foreground_session_cap: usize,
        active_background_sessions: usize,
        background_session_cap: usize,
        live_source_groups: usize,
        source_group_cap: usize,
        live_lane_actors: usize,
        lane_actor_cap: usize,
        retiring_lane_actors: usize,
        post_drop_active_sessions: usize,
    }

    #[derive(Clone, Serialize)]
    struct Phase1LatencySample {
        trial: usize,
        sequence_index: usize,
        source_count: usize,
        requested_source_tick: i64,
        decoded_source_ticks: Vec<i64>,
        decoded_media_ids: Vec<u32>,
        output_size: [u32; 2],
        observed_decoder_backends: Vec<String>,
        input_to_submit_us: u128,
        frame_ready_ms: u128,
        active_sticky_sessions: usize,
        peak_sticky_sessions: usize,
        session_cap: usize,
        post_drop_active_sessions: usize,
    }

    #[derive(Clone, Copy, Serialize)]
    struct Phase1LatencyDistribution {
        p50: u128,
        p95: u128,
        max: u128,
    }

    #[derive(Serialize)]
    struct Phase1LatencyScenario {
        source_count: usize,
        samples: Vec<Phase1LatencySample>,
        input_to_submit_us: Phase1LatencyDistribution,
        frame_ready_ms: Phase1LatencyDistribution,
    }

    #[derive(Serialize)]
    struct Phase1LatencyComparison {
        input_to_submit_p95_delta_us: i128,
        input_to_submit_p95_ratio: Option<f64>,
        frame_ready_p95_delta_ms: i128,
        frame_ready_p95_ratio: Option<f64>,
    }

    #[derive(Serialize)]
    struct Phase1LatencyReport {
        schema_version: u32,
        status: &'static str,
        trial_count_per_scenario: usize,
        input_to_submit_p95_us_limit: u128,
        sources: Vec<Phase1MultisourceSource>,
        output_size: [u32; 2],
        one_source: Phase1LatencyScenario,
        four_source: Phase1LatencyScenario,
        comparison: Phase1LatencyComparison,
    }

    #[derive(Serialize)]
    struct Phase1SustainedReport {
        schema_version: u32,
        status: &'static str,
        requested_duration_seconds: u64,
        actual_duration_seconds: f64,
        authoritative: bool,
        source_count: usize,
        sources: Vec<Phase1MultisourceSource>,
        output_size: [u32; 2],
        cycle_count: u64,
        source_exercise_counts: [u64; MONITOR_LAYER_COUNT],
        requested_tick_pattern: [i64; 8],
        max_decoded_tick_delta_us: i64,
        monitor_dropped_frame_limit: u64,
        input_to_submit_p95_us_limit: u128,
        input_to_submit_samples_us: Vec<u128>,
        input_to_submit_us: Phase1LatencyDistribution,
        frame_ready_samples_ms: Vec<u128>,
        frame_ready_ms: Phase1LatencyDistribution,
        runtime_diagnostics_delta: RuntimeDiagnosticsReport,
        monitor_resources: PlaybackSoakMonitorResources,
        observed_decoder_backends: Vec<String>,
        post_drop_active_sessions: usize,
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    struct Phase1LiveAudioCounterDelta {
        callback_lock_failures: u64,
        underrun_device_frames: u64,
        late_decoded_frames_discarded: u64,
    }

    #[derive(Serialize)]
    struct Phase1LiveAudioReport {
        schema_version: u32,
        status: &'static str,
        requested_duration_seconds: u64,
        actual_duration_seconds: f64,
        source_count: usize,
        video_sources: Vec<Phase1MultisourceSource>,
        audio_source: Phase1MultisourceSource,
        clip_duration_ticks: i64,
        audio_target_count: usize,
        source_exercise_counts: [u64; MONITOR_LAYER_COUNT],
        slow_layer: usize,
        slow_request_id: u64,
        requested_blocked_duration_ms: u128,
        actual_blocked_duration_ms: u128,
        minimum_actual_blocked_duration_ms: u128,
        ready_source_presentations_during_block: u64,
        minimum_ready_source_presentations_during_block: u64,
        audio_tick_delta_during_block: i64,
        minimum_audio_tick_delta_during_block: i64,
        slow_source_presentations_after_release: u64,
        minimum_slow_source_presentations_after_release: u64,
        slow_source_recovered: bool,
        source_tick_start: i64,
        source_tick_end: i64,
        source_tick_delta: i64,
        expected_source_tick_delta: i64,
        clock_drift_us: i64,
        clock_drift_limit_us: i64,
        callback_sample_delta: u64,
        mix_sample_delta: u64,
        max_device_clock_stall_ms: u128,
        max_device_clock_stall_limit_ms: u128,
        warmup_max_meter: f32,
        max_meter: f32,
        final_meter: f32,
        meter_observation_count: u64,
        nonzero_meter_observation_count: u64,
        minimum_nonzero_meter_observations: u64,
        monitor_request_count: u64,
        minimum_monitor_request_count: u64,
        minimum_presentations_per_source: u64,
        input_to_submit_p95_us_limit: u128,
        input_to_submit_samples_us: Vec<u128>,
        input_to_submit_us: Phase1LatencyDistribution,
        runtime_diagnostics_delta: RuntimeDiagnosticsReport,
        audio_counter_delta: Phase1LiveAudioCounterDelta,
        transport_lost: bool,
        audio_error: Option<String>,
        monitor_resources: PlaybackSoakMonitorResources,
        observed_decoder_backends: Vec<String>,
        post_drop_active_sessions: usize,
    }

    #[derive(Serialize)]
    struct Phase1GenerationStressIdentity {
        layer: usize,
        generation: u64,
        request_id: u64,
        media_id: u32,
        source_tick: i64,
    }

    #[derive(Serialize)]
    struct Phase1GenerationStressCycle {
        cycle: usize,
        toggled_layer: usize,
        forward_playhead_tick: i64,
        backward_playhead_tick: i64,
        disabled_frame_cleared: bool,
        unaffected_layers_retained: bool,
        forward_identities: Vec<Phase1GenerationStressIdentity>,
        disabled_identities: Vec<Phase1GenerationStressIdentity>,
        latest_identities: Vec<Phase1GenerationStressIdentity>,
        final_applied_identities: [Option<(u64, u64)>; MONITOR_LAYER_COUNT],
        captured_real_frame_replay_rejected: bool,
    }

    #[derive(Serialize)]
    struct Phase1GenerationStressOperations {
        forward_submits: usize,
        backward_submits: usize,
        disable_operations: usize,
        reenable_operations: usize,
        barrier_supersessions: usize,
    }

    #[derive(Serialize)]
    struct Phase1GenerationStressStaleRejection {
        barrier_blocked: bool,
        barrier_request_id: u64,
        captured_real_frame_identity: (u64, u64),
        captured_real_frame_replayed_after_generation: bool,
        captured_real_frame_rejected: bool,
        matching_generation_control_presented: bool,
        control_generation: u64,
    }

    #[derive(Serialize)]
    struct Phase1GenerationStressPostDrop {
        active_sessions: usize,
        live_source_groups: usize,
        live_lane_actors: usize,
        retiring_lane_actors: usize,
    }

    #[derive(Serialize)]
    struct Phase1GenerationStressReport {
        schema_version: u32,
        status: &'static str,
        source_count: usize,
        cycles: usize,
        sources: Vec<Phase1MultisourceSource>,
        output_size: [u32; 2],
        operations: Phase1GenerationStressOperations,
        observed_decoder_backends: Vec<String>,
        stale_rejection: Phase1GenerationStressStaleRejection,
        per_cycle: Vec<Phase1GenerationStressCycle>,
        runtime_diagnostics_delta: RuntimeDiagnosticsReport,
        resources_valid: bool,
        resource_checkpoint_count: usize,
        resources: PlaybackSoakMonitorResources,
        post_drop: Phase1GenerationStressPostDrop,
    }

    fn phase0_required_absolute_path(name: &str) -> Result<PathBuf, String> {
        let path = std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{name} is required"))?;
        if !path.is_absolute() {
            return Err(format!("{name} must be an absolute path"));
        }
        Ok(path)
    }

    fn phase0_required_absolute_file(name: &str) -> Result<PathBuf, String> {
        let path = phase0_required_absolute_path(name)?;
        if !path.is_file() {
            return Err(format!("{name} does not name a file: {}", path.display()));
        }
        Ok(path)
    }

    fn phase1_multisource_sources() -> Result<Vec<Phase1MultisourceSource>, String> {
        [
            "MAELSTROM_TEST_MEDIA",
            "MAELSTROM_TEST_MEDIA_SECOND",
            "MAELSTROM_TEST_MEDIA_THIRD",
            "MAELSTROM_TEST_MEDIA_FOURTH",
        ]
        .into_iter()
        .map(|name| {
            let path = phase0_required_absolute_file(name)?
                .canonicalize()
                .map_err(|error| format!("could not canonicalize {name}: {error}"))?;
            let size_bytes = fs::metadata(&path)
                .map_err(|error| format!("could not stat {name}: {error}"))?
                .len();
            if size_bytes == 0 {
                return Err(format!("{name} names an empty source: {}", path.display()));
            }
            Ok(Phase1MultisourceSource { path, size_bytes })
        })
        .collect()
    }

    fn phase1_multisource_report_path() -> Result<PathBuf, String> {
        let path = phase0_required_absolute_path("MAELSTROM_PHASE1_MULTISOURCE_REPORT")?;
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            return Err("MAELSTROM_PHASE1_MULTISOURCE_REPORT must end in .json".to_owned());
        }
        let parent = path.parent().ok_or_else(|| {
            "MAELSTROM_PHASE1_MULTISOURCE_REPORT has no parent directory".to_owned()
        })?;
        if !parent.is_dir() {
            return Err(format!(
                "MAELSTROM_PHASE1_MULTISOURCE_REPORT parent does not exist: {}",
                parent.display()
            ));
        }
        Ok(path)
    }

    fn phase1_generation_stress_report_path() -> Result<PathBuf, String> {
        let path = phase0_required_absolute_path("MAELSTROM_PHASE1_GENERATION_STRESS_REPORT")?;
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            return Err("MAELSTROM_PHASE1_GENERATION_STRESS_REPORT must end in .json".to_owned());
        }
        let parent = path.parent().ok_or_else(|| {
            "MAELSTROM_PHASE1_GENERATION_STRESS_REPORT has no parent directory".to_owned()
        })?;
        if !parent.is_dir() {
            return Err(format!(
                "MAELSTROM_PHASE1_GENERATION_STRESS_REPORT parent does not exist: {}",
                parent.display()
            ));
        }
        Ok(path)
    }

    fn phase1_latency_report_path() -> Result<PathBuf, String> {
        let path = phase0_required_absolute_path("MAELSTROM_PHASE1_LATENCY_REPORT")?;
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            return Err("MAELSTROM_PHASE1_LATENCY_REPORT must end in .json".to_owned());
        }
        let parent = path
            .parent()
            .ok_or_else(|| "MAELSTROM_PHASE1_LATENCY_REPORT has no parent directory".to_owned())?;
        if !parent.is_dir() {
            return Err(format!(
                "MAELSTROM_PHASE1_LATENCY_REPORT parent does not exist: {}",
                parent.display()
            ));
        }
        Ok(path)
    }

    fn phase1_sustained_report_path() -> Result<PathBuf, String> {
        let path = phase0_required_absolute_path("MAELSTROM_PHASE1_SUSTAINED_REPORT")?;
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            return Err("MAELSTROM_PHASE1_SUSTAINED_REPORT must end in .json".to_owned());
        }
        let parent = path.parent().ok_or_else(|| {
            "MAELSTROM_PHASE1_SUSTAINED_REPORT has no parent directory".to_owned()
        })?;
        if !parent.is_dir() {
            return Err(format!(
                "MAELSTROM_PHASE1_SUSTAINED_REPORT parent does not exist: {}",
                parent.display()
            ));
        }
        Ok(path)
    }

    fn phase1_live_audio_report_path() -> Result<PathBuf, String> {
        let path = phase0_required_absolute_path("MAELSTROM_PHASE1_LIVE_AUDIO_REPORT")?;
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            return Err("MAELSTROM_PHASE1_LIVE_AUDIO_REPORT must end in .json".to_owned());
        }
        let parent = path.parent().ok_or_else(|| {
            "MAELSTROM_PHASE1_LIVE_AUDIO_REPORT has no parent directory".to_owned()
        })?;
        if !parent.is_dir() {
            return Err(format!(
                "MAELSTROM_PHASE1_LIVE_AUDIO_REPORT parent does not exist: {}",
                parent.display()
            ));
        }
        Ok(path)
    }

    fn phase1_live_audio_source() -> Result<Phase1MultisourceSource, String> {
        let path = phase0_required_absolute_file("MAELSTROM_PHASE1_AUDIO_MEDIA")?
            .canonicalize()
            .map_err(|error| {
                format!("could not canonicalize MAELSTROM_PHASE1_AUDIO_MEDIA: {error}")
            })?;
        let size_bytes = fs::metadata(&path)
            .map_err(|error| format!("could not stat MAELSTROM_PHASE1_AUDIO_MEDIA: {error}"))?
            .len();
        if size_bytes == 0 {
            return Err(format!(
                "MAELSTROM_PHASE1_AUDIO_MEDIA names an empty source: {}",
                path.display()
            ));
        }
        Ok(Phase1MultisourceSource { path, size_bytes })
    }

    fn phase1_sustained_duration_seconds(value: Option<&str>) -> u64 {
        value
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PHASE1_SUSTAINED_SOAK_SECONDS)
            .clamp(
                MIN_PHASE1_SUSTAINED_SOAK_SECONDS,
                MAX_PHASE1_SUSTAINED_SOAK_SECONDS,
            )
    }

    fn phase1_live_audio_duration_seconds(value: Option<&str>) -> u64 {
        value
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PHASE1_LIVE_AUDIO_SECONDS)
            .clamp(MIN_PHASE1_LIVE_AUDIO_SECONDS, MAX_PHASE1_LIVE_AUDIO_SECONDS)
    }

    fn phase1_sustained_dropped_frame_limit(expected_requests: u64) -> u64 {
        expected_requests.div_ceil(1_000).max(4)
    }

    fn phase1_live_audio_resources_are_bounded(resources: &PlaybackSoakMonitorResources) -> bool {
        resources.current_frame_cache_bytes <= resources.frame_cache_capacity_bytes
            && resources.peak_frame_cache_bytes_upper_bound <= resources.frame_cache_capacity_bytes
            && resources.active_sticky_sessions
                == resources.active_foreground_sessions + resources.active_background_sessions
            && resources.session_cap
                == resources.foreground_session_cap + resources.background_session_cap
            && resources.active_sticky_sessions <= resources.session_cap
            && resources.peak_sticky_sessions <= resources.session_cap
            && resources.active_foreground_sessions <= resources.foreground_session_cap
            && resources.active_background_sessions <= resources.background_session_cap
            && resources.live_source_groups <= resources.source_group_cap
            && resources.live_lane_actors + resources.retiring_lane_actors
                <= resources.lane_actor_cap
    }

    fn phase1_multisource_app(sources: &[Phase1MultisourceSource], source_count: usize) -> App {
        assert!((1..=MONITOR_LAYER_COUNT).contains(&source_count));
        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths(
            sources
                .iter()
                .take(source_count)
                .map(|source| source.path.clone()),
        );
        let mut video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while video_tracks.len() < source_count {
            video_tracks.push(
                app.editor
                    .timeline
                    .add_track(nle_timeline::TrackKind::Video),
            );
        }
        for (track, media_id) in video_tracks.into_iter().zip(1..=source_count as u32) {
            app.editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(5_000_000),
                    nle_timeline::Tick(0),
                )
                .expect("insert Phase 1 multisource fixture clip");
        }
        app
    }

    fn phase1_latency_trial(
        sources: &[Phase1MultisourceSource],
        trial: usize,
        sequence_index: usize,
        source_count: usize,
        requested_source_tick: i64,
    ) -> Phase1LatencySample {
        assert!((1..=MONITOR_LAYER_COUNT).contains(&source_count));
        let mut app = App::new_with_catalog(false, None);
        app.editor.add_media_paths(
            sources
                .iter()
                .take(source_count)
                .map(|source| source.path.clone()),
        );
        let mut video_tracks = app
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == nle_timeline::TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while video_tracks.len() < source_count {
            video_tracks.push(
                app.editor
                    .timeline
                    .add_track(nle_timeline::TrackKind::Video),
            );
        }
        for (track, media_id) in video_tracks.into_iter().zip(1..=source_count as u32) {
            app.editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(2_000_000),
                    nle_timeline::Tick(0),
                )
                .expect("insert Phase 1 latency fixture clip");
        }

        // A fresh App per trial prevents a prior scenario's sticky sessions or frame cache from
        // making the one- and four-source scheduler paths incomparable.
        let input_started_at = Instant::now();
        app.editor
            .set_playhead(nle_timeline::Tick(requested_source_tick));
        app.editor.set_preview_quality(PreviewQuality::Full);
        app.editor.set_paused_preview_quality(PreviewQuality::Full);
        let mut preview = preview_request(&app.editor);
        assert_eq!(preview.selected_quality, PreviewQuality::Full);
        assert_eq!(preview.resolved_quality, PreviewQuality::Full);
        assert_eq!(preview.playhead_tick, requested_source_tick);
        preview.output_size = [1920, 1080];
        app.submit_monitor_decode_request(preview);
        let input_to_submit_us = input_started_at.elapsed().as_micros();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && (0..source_count).any(|layer| app.editor.monitor_frame_for_layer(layer).is_none())
        {
            app.poll_monitor_decoder();
            thread::sleep(Duration::from_millis(5));
        }
        let frame_ready_ms = input_started_at.elapsed().as_millis();
        let frames = (0..source_count)
            .map(|layer| {
                app.editor
                    .monitor_frame_for_layer(layer)
                    .expect("matching monitor frame")
            })
            .collect::<Vec<_>>();
        let decoded_media_ids = frames
            .iter()
            .map(|frame| frame.media_id.expect("matching decoded media ID"))
            .collect::<Vec<_>>();
        assert_eq!(
            decoded_media_ids,
            (1..=source_count as u32).collect::<Vec<_>>(),
            "trial decoded unrelated media IDs"
        );
        let decoded_source_ticks = frames
            .iter()
            .map(|frame| {
                assert_eq!((frame.width, frame.height), (1920, 1080));
                let tick = frame.source_tick.expect("matching decoded source tick").0;
                assert!(
                    tick >= requested_source_tick,
                    "decoded source tick {tick} preceded requested mid-GOP tick {requested_source_tick}"
                );
                assert!(
                    tick <= requested_source_tick + 33_334,
                    "decoded source tick {tick} exceeded one 30fps frame after requested tick {requested_source_tick}"
                );
                tick
            })
            .collect::<Vec<_>>();
        assert!(
            app.monitor_latest_request_ids[..source_count]
                .iter()
                .all(|id| *id > 0)
        );
        assert!(
            app.monitor_requests_in_flight[..source_count]
                .iter()
                .all(|active| !active)
        );
        assert!(!app.observed_decoder_backends.is_empty());
        let observed_decoder_backends = app.observed_decoder_backends.clone();
        let diagnostics = app.monitor_session_pool.diagnostics();
        let monitor_session_pool = app.monitor_session_pool.clone();
        drop(app);
        let release_deadline = Instant::now() + Duration::from_secs(5);
        let post_drop_active_sessions = loop {
            let active = monitor_session_pool.diagnostics().active_sticky_sessions;
            if active == 0 {
                break active;
            }
            assert!(
                Instant::now() < release_deadline,
                "monitor decoder sessions remained active after latency trial App drop: {active}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        Phase1LatencySample {
            trial,
            sequence_index,
            source_count,
            requested_source_tick,
            decoded_source_ticks,
            decoded_media_ids,
            output_size: [1920, 1080],
            observed_decoder_backends,
            input_to_submit_us,
            frame_ready_ms,
            active_sticky_sessions: diagnostics.active_sticky_sessions,
            peak_sticky_sessions: diagnostics.peak_sticky_sessions,
            session_cap: diagnostics.session_cap,
            post_drop_active_sessions,
        }
    }

    fn phase1_latency_distribution(
        values: impl Iterator<Item = u128>,
    ) -> Phase1LatencyDistribution {
        let mut values = values.collect::<Vec<_>>();
        assert!(!values.is_empty(), "latency distribution requires samples");
        values.sort_unstable();
        let nearest_rank = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
        Phase1LatencyDistribution {
            p50: nearest_rank(50),
            p95: nearest_rank(95),
            max: *values.last().expect("nonempty sorted samples"),
        }
    }

    fn phase1_latency_summary(samples: &[Phase1LatencySample]) -> Phase1LatencyScenario {
        assert!(!samples.is_empty());
        let source_count = samples[0].source_count;
        assert!(
            samples
                .iter()
                .all(|sample| sample.source_count == source_count)
        );
        Phase1LatencyScenario {
            source_count,
            samples: samples.to_vec(),
            input_to_submit_us: phase1_latency_distribution(
                samples.iter().map(|sample| sample.input_to_submit_us),
            ),
            frame_ready_ms: phase1_latency_distribution(
                samples.iter().map(|sample| sample.frame_ready_ms),
            ),
        }
    }

    fn phase1_latency_ratio(numerator: u128, denominator: u128) -> Option<f64> {
        (denominator != 0).then(|| numerator as f64 / denominator as f64)
    }

    fn phase1_multisource_write_report<T: Serialize>(
        path: &Path,
        report: &T,
    ) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!(
            "{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            file.write_all(&encoded)
                .map_err(|error| error.to_string())?;
            file.flush().map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            nle_project_io::replace_file(&temporary, path).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn phase0_report_path() -> Result<PathBuf, String> {
        let report = phase0_required_absolute_path("MAELSTROM_PHASE0_REPORT")?;
        if report.extension().and_then(|extension| extension.to_str()) != Some("json") {
            return Err("MAELSTROM_PHASE0_REPORT must end in .json".to_owned());
        }
        let artifact_root = phase0_required_absolute_path("MAELSTROM_PHASE0_ARTIFACT_ROOT")?
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let report_parent = report
            .parent()
            .ok_or_else(|| "MAELSTROM_PHASE0_REPORT has no parent directory".to_owned())?
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if report_parent != artifact_root
            || !artifact_root.ends_with(Path::new("artifacts").join("phase0-scenarios"))
        {
            return Err(format!(
                "MAELSTROM_PHASE0_REPORT must be directly inside the supplied artifacts/phase0-scenarios directory, got {}",
                report_parent.display()
            ));
        }
        Ok(report)
    }

    fn phase0_run_scenario(
        name: &'static str,
        iterations: u32,
        backend_role: Option<Phase0BackendRole>,
        run: impl FnOnce() -> Result<(Option<String>, String), String>,
    ) -> Phase0ScenarioReport {
        let started = Instant::now();
        match run() {
            Ok((backend, evidence)) => {
                let (decoder_backend, encoder_backend) =
                    phase0_backend_fields(backend_role, backend);
                Phase0ScenarioReport {
                    name,
                    iterations,
                    elapsed_ms: started.elapsed().as_millis(),
                    decoder_backend,
                    encoder_backend,
                    passed: true,
                    evidence,
                }
            }
            Err(evidence) => Phase0ScenarioReport {
                name,
                iterations,
                elapsed_ms: started.elapsed().as_millis(),
                decoder_backend: None,
                encoder_backend: None,
                passed: false,
                evidence,
            },
        }
    }

    fn phase0_backend_fields(
        backend_role: Option<Phase0BackendRole>,
        backend: Option<String>,
    ) -> (Option<String>, Option<String>) {
        match backend_role {
            Some(Phase0BackendRole::Decoder) => (backend, None),
            Some(Phase0BackendRole::Encoder) => (None, backend),
            None => (None, None),
        }
    }

    fn phase0_write_report(path: &Path, report: &Phase0Report) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!(
            "{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            file.write_all(&encoded)
                .map_err(|error| error.to_string())?;
            file.flush().map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            nle_project_io::replace_file(&temporary, path).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn phase0_video_strip(media_id: u32) -> Arc<nle_waveform::VideoStrip> {
        debug_assert_eq!(
            PHASE0_VIDEO_STRIP_WIDTH as usize * PHASE0_VIDEO_STRIP_HEIGHT as usize * 4,
            PHASE0_VIDEO_STRIP_BYTES
        );
        Arc::new(nle_waveform::VideoStrip {
            width: PHASE0_VIDEO_STRIP_WIDTH,
            height: PHASE0_VIDEO_STRIP_HEIGHT,
            // A non-zero fill forces deterministic physical page commitment; a zero-filled
            // allocation could otherwise remain backed by the operating system's shared zero page.
            rgba: vec![media_id as u8; PHASE0_VIDEO_STRIP_BYTES],
            duration_seconds: 1.0,
            frame_count: 1,
            frame_width: PHASE0_VIDEO_STRIP_WIDTH,
            frame_height: PHASE0_VIDEO_STRIP_HEIGHT,
            columns: 1,
            rows: 1,
        })
    }

    fn phase0_wait_for_monitor_event(
        decoder: &nle_decode::MonitorDecoder,
        request_id: u64,
    ) -> Result<nle_decode::DecodeEvent, String> {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(event) = decoder.try_recv().map_err(|error| error.to_string())? {
                let current = match &event {
                    nle_decode::DecodeEvent::Frame(frame) => frame.request_id == request_id,
                    nle_decode::DecodeEvent::Error(error) => error.request_id == request_id,
                };
                if current {
                    return Ok(event);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err(format!(
            "monitor decode request {request_id} did not complete before deadline"
        ))
    }

    fn phase0_export_artifacts(output: &Path) -> Vec<PathBuf> {
        let Some(parent) = output.parent() else {
            return Vec::new();
        };
        let stem = output
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let output_name = output.file_name();
        fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                    return false;
                };
                path.file_name() == output_name
                    || (name.starts_with(&format!(".{stem}.maelstrom-"))
                        && (name.ends_with(".staged.mp4") || name.ends_with(".filter")))
                    || (name.starts_with(".maelstrom-export-") && name.ends_with(".filter"))
            })
            .collect()
    }

    #[test]
    fn phase0_backend_roles_are_reported_in_explicit_fields() {
        assert_eq!(
            phase0_backend_fields(Some(Phase0BackendRole::Decoder), Some("D3D11VA".to_owned()),),
            (Some("D3D11VA".to_owned()), None)
        );
        assert_eq!(
            phase0_backend_fields(Some(Phase0BackendRole::Encoder), Some("D3D11VA".to_owned()),),
            (None, Some("D3D11VA".to_owned()))
        );
        assert_eq!(
            phase0_backend_fields(None, Some("ignored".to_owned())),
            (None, None)
        );
    }

    #[test]
    fn phase0_report_serializes_separate_backend_fields() {
        let report = Phase0ScenarioReport {
            name: "decoder",
            iterations: 1,
            elapsed_ms: 0,
            decoder_backend: Some("Software".to_owned()),
            encoder_backend: None,
            passed: true,
            evidence: "test".to_owned(),
        };
        let value = serde_json::to_value(report).expect("serialize Phase 0 report");
        assert_eq!(value["decoder_backend"], "Software");
        assert!(value["encoder_backend"].is_null());
        assert!(value.get("backend").is_none());
    }

    #[test]
    #[ignore = "requires explicit MAELSTROM_PHASE0_MEDIA and MAELSTROM_PHASE0_REPORT"]
    fn phase0_scenario_matrix() {
        let media = phase0_required_absolute_file("MAELSTROM_PHASE0_MEDIA")
            .expect("validate Phase 0 media fixture");
        let report_path = phase0_report_path().expect("validate Phase 0 report path");
        let ffmpeg_root = std::env::var_os("FFMPEG_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.join("bin/ffmpeg.exe").is_file())
            .expect("FFMPEG_DIR must name an absolute pinned FFmpeg bundle");
        assert!(ffmpeg_root.join("bin/ffprobe.exe").is_file());
        let output = report_path
            .parent()
            .expect("report parent")
            .join("phase0-cancelled.mp4");
        let _ = fs::remove_file(&output);

        let mut scenarios = Vec::new();
        scenarios.push(phase0_run_scenario(
            "reverse_scrub_public_monitor_decoder",
            6,
            Some(Phase0BackendRole::Decoder),
            || {
                let decoder = nle_decode::MonitorDecoder::new_with_notifier_and_cache_bytes(
                    || {},
                    16 * 1024 * 1024,
                );
                const REQUESTED_TICK: i64 = 300_000;
                const SOURCE_FRAME_DURATION_TICK: i64 = 33_367;
                let source_ticks = [
                    1_800_000,
                    1_500_000,
                    1_200_000,
                    900_000,
                    600_000,
                    REQUESTED_TICK,
                ];
                for (index, source_tick) in source_ticks.into_iter().enumerate() {
                    decoder
                        .request(nle_decode::DecodeRequest {
                            project_epoch: 1,
                            cache_epoch: 1,
                            request_id: index as u64 + 1,
                            media_id: 1,
                            path: media.clone(),
                            source_tick,
                            width: 160,
                            height: 90,
                            is_scrubbing: true,
                            prewarm_scrub_workers: false,
                            high_quality_scaling: false,
                            progressive_scrub_frames: true,
                            source_frame_duration_tick: Some(SOURCE_FRAME_DURATION_TICK),
                            acceleration: nle_decode::AccelerationPreference::Software,
                        })
                        .map_err(|error| error.to_string())?;
                }
                let deadline = Instant::now() + Duration::from_secs(8);
                let mut backend = None;
                let mut completed = false;
                while Instant::now() < deadline {
                    if let Some(event) = decoder.try_recv().map_err(|error| error.to_string())? {
                        match event {
                            nle_decode::DecodeEvent::Frame(frame) if frame.request_id == 6 => {
                                if (frame.width, frame.height) != (160, 90) {
                                    return Err(format!(
                                        "final reverse scrub dimensions were {}x{}, expected 160x90",
                                        frame.width, frame.height
                                    ));
                                }
                                if !(REQUESTED_TICK..=REQUESTED_TICK + SOURCE_FRAME_DURATION_TICK)
                                    .contains(&frame.source_tick)
                                {
                                    return Err(format!(
                                        "final reverse scrub source tick {} did not reach requested {} within one declared source frame",
                                        frame.source_tick, REQUESTED_TICK
                                    ));
                                }
                                backend = frame
                                    .backend
                                    .map(|value| value.display_name().to_owned());
                                if backend.is_none() {
                                    return Err("final reverse scrub frame did not expose a decoder backend".to_owned());
                                }
                                completed = true;
                                break;
                            }
                            nle_decode::DecodeEvent::Error(error) if error.request_id == 6 => {
                                return Err(format!("final reverse scrub decode failed: {}", error.message));
                            }
                            _ => {}
                        }
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if !completed {
                    return Err("final reverse scrub request did not complete before deadline".to_owned());
                }
                drop(decoder);
                Ok((backend, "six decreasing source ticks accepted by the public latest-wins decoder; the final 160x90 frame reached 300000 microseconds or the next declared source frame".to_owned()))
            },
        ));
        scenarios.push(phase0_run_scenario("rapid_editor_state_switching", 8, None, || {
            let mut first = EditorState::new(Language::English, "Phase 0 A");
            first.add_media_paths([media.clone()]);
            if !first.add_selected_to_timeline() {
                return Err("could not place first project fixture".to_owned());
            }
            let first_snapshot = first.snapshot();
            let mut second = EditorState::new(Language::English, "Phase 0 B");
            second.add_media_paths([media.clone()]);
            if !second.add_selected_to_timeline() {
                return Err("could not place second project fixture".to_owned());
            }
            second.set_playhead(nle_timeline::Tick(500_000));
            let second_snapshot = second.snapshot();
            let mut app = App::new_with_catalog(false, None);
            for index in 0..8 {
                let (name, snapshot, expected_tick) = if index % 2 == 0 {
                    ("Phase 0 A", first_snapshot.clone(), 0)
                } else {
                    ("Phase 0 B", second_snapshot.clone(), 500_000)
                };
                app.show_editor_screen(name.to_owned(), Language::English, Some(snapshot), ProjectSettings::default(), false);
                if app.editor.playhead.0 != expected_tick || app.editor.media.first().map(|item| &item.path) != Some(&media) {
                    return Err(format!("editor state switch {index} did not restore the expected snapshot"));
                }
            }
            Ok((None, "eight alternating App editor restores retained the fixture path and each snapshot playhead".to_owned()))
        }));
        scenarios.push(phase0_run_scenario("offline_media_detection_and_recovery", 1, Some(Phase0BackendRole::Decoder), || {
            let restored = report_path
                .parent()
                .expect("report parent")
                .join("phase0-offline-recovery.mp4");
            let unavailable = restored.with_extension("mp4.unavailable");
            let _ = fs::remove_file(&restored);
            let _ = fs::remove_file(&unavailable);
            let result = (|| {
                fs::copy(&media, &restored).map_err(|error| error.to_string())?;
                let mut editor = EditorState::new(Language::English, "Phase 0 offline");
                editor.add_media_paths([restored.clone()]);
                fs::rename(&restored, &unavailable).map_err(|error| error.to_string())?;
                let missing_decoder = nle_decode::MonitorDecoder::new();
                missing_decoder
                    .request(nle_decode::DecodeRequest {
                        project_epoch: 1,
                        cache_epoch: 1,
                        request_id: 1,
                        media_id: 1,
                        path: restored.clone(),
                        source_tick: 300_000,
                        width: 160,
                        height: 90,
                        is_scrubbing: false,
                        prewarm_scrub_workers: false,
                        high_quality_scaling: false,
                        progressive_scrub_frames: false,
                        source_frame_duration_tick: Some(33_367),
                        acceleration: nle_decode::AccelerationPreference::Software,
                    })
                    .map_err(|error| error.to_string())?;
                match phase0_wait_for_monitor_event(&missing_decoder, 1)? {
                    nle_decode::DecodeEvent::Error(_) => {}
                    nle_decode::DecodeEvent::Frame(_) => {
                        return Err("removed fixture unexpectedly decoded before offline state was recorded".to_owned());
                    }
                }
                drop(missing_decoder);
                editor.set_media_error(1, "fixture absent during decoder probe");
                if !editor.media_is_offline(1) {
                    return Err("editor did not expose the offline media state".to_owned());
                }
                fs::rename(&unavailable, &restored).map_err(|error| error.to_string())?;
                editor.set_media_metadata(1, nle_ui_core::MediaMetadata {
                    file_size: fs::metadata(&restored).ok().map(|metadata| metadata.len()),
                    ..Default::default()
                });
                if editor.media_is_offline(1) {
                    return Err("editor did not clear offline state after media metadata recovery".to_owned());
                }
                let recovery_decoder = nle_decode::MonitorDecoder::new();
                recovery_decoder
                    .request(nle_decode::DecodeRequest {
                        project_epoch: 1,
                        cache_epoch: 2,
                        request_id: 2,
                        media_id: 1,
                        path: restored.clone(),
                        source_tick: 300_000,
                        width: 160,
                        height: 90,
                        is_scrubbing: false,
                        prewarm_scrub_workers: false,
                        high_quality_scaling: false,
                        progressive_scrub_frames: false,
                        source_frame_duration_tick: Some(33_367),
                        acceleration: nle_decode::AccelerationPreference::Software,
                    })
                    .map_err(|error| error.to_string())?;
                let recovered = phase0_wait_for_monitor_event(&recovery_decoder, 2)?;
                drop(recovery_decoder);
                match recovered {
                    nle_decode::DecodeEvent::Frame(frame) if (frame.width, frame.height) == (160, 90) => {
                        let backend = frame
                            .backend
                            .map(|value| value.display_name().to_owned())
                            .ok_or_else(|| "restored fixture frame did not expose a decoder backend".to_owned())?;
                        Ok((Some(backend), "the fixture was removed and produced a real decoder error, EditorState exposed offline=true, then the file was restored, offline=false, and a fresh decoder returned 160x90".to_owned()))
                    }
                    nle_decode::DecodeEvent::Frame(frame) => Err(format!("restored fixture decoded at unexpected {}x{} dimensions", frame.width, frame.height)),
                    nle_decode::DecodeEvent::Error(error) => Err(format!("restored fixture decoder recovery failed: {}", error.message)),
                }
            })();
            if unavailable.exists() {
                let _ = fs::rename(&unavailable, &restored);
            }
            let _ = fs::remove_file(&restored);
            result
        }));
        scenarios.push(phase0_run_scenario(
            "runtime_video_strip_cache_eviction",
            5,
            None,
            || {
                let mut app = App::new_with_catalog(false, None);
                let mut cumulative_bytes = 0usize;
                let mut peak_live_bytes = 0usize;
                for media_id in 1..=5 {
                    let strip = phase0_video_strip(media_id);
                    let strip_bytes = strip.rgba.len();
                    cumulative_bytes = cumulative_bytes.saturating_add(strip_bytes);
                    // This includes the new strip before `retain_video_strip` can release an
                    // evicted strip, so it captures the pressure peak rather than only retention.
                    peak_live_bytes = peak_live_bytes.max(app.video_strip_bytes + strip_bytes);
                    app.retain_video_strip(media_id, strip);
                    if app.video_strip_bytes > MAX_RUNTIME_VIDEO_STRIP_BYTES {
                        return Err(format!(
                            "runtime video-strip cache exceeded its hard cap after insertion {media_id}: {} > {}",
                            app.video_strip_bytes, MAX_RUNTIME_VIDEO_STRIP_BYTES
                        ));
                    }
                    let expected_ids: &[u32] = match media_id {
                        1 => &[1],
                        2 => &[1, 2],
                        3 => &[1, 2, 3],
                        4 => &[2, 3, 4],
                        5 => &[3, 4, 5],
                        _ => unreachable!("Phase 0 strip checkpoint has five insertions"),
                    };
                    let retained_ids: Vec<u32> = app.video_strip_order.iter().copied().collect();
                    let expected_bytes = expected_ids.len() * PHASE0_VIDEO_STRIP_BYTES;
                    if retained_ids != expected_ids || app.video_strip_bytes != expected_bytes {
                        return Err(format!(
                            "runtime video-strip cache retained unexpected entries after insertion {media_id}: retained_ids={retained_ids:?} retained_bytes={} expected_ids={expected_ids:?} expected_bytes={expected_bytes}",
                            app.video_strip_bytes
                        ));
                    }
                }
                let retained_ids: Vec<u32> = app.video_strip_order.iter().copied().collect();
                let expected_retained_bytes = PHASE0_VIDEO_STRIP_BYTES * 3;
                if retained_ids != [3, 4, 5]
                    || app.video_strips.len() != 3
                    || app.video_strips.contains_key(&1)
                    || app.video_strips.contains_key(&2)
                    || app.video_strip_bytes != expected_retained_bytes
                {
                    return Err(format!(
                        "runtime video-strip cache did not evict exact oldest entries: retained_ids={retained_ids:?} retained_bytes={} expected_bytes={expected_retained_bytes}",
                        app.video_strip_bytes
                    ));
                }
                Ok((
                    None,
                    format!(
                        "cumulative_bytes={cumulative_bytes} retained_bytes={} cap_bytes={} peak_live_bytes={peak_live_bytes}; five deterministic {}-byte strips retained IDs {retained_ids:?} after exact oldest eviction",
                        app.video_strip_bytes,
                        MAX_RUNTIME_VIDEO_STRIP_BYTES,
                        PHASE0_VIDEO_STRIP_BYTES,
                    ),
                ))
            },
        ));
        scenarios.push(phase0_run_scenario(
            "four_source_decoded_frame_cache_pressure",
            4,
            Some(Phase0BackendRole::Decoder),
            || {
                const SOURCE_COUNT: usize = 4;
                const FRAME_WIDTH: u32 = 160;
                const FRAME_HEIGHT: u32 = 90;
                const FRAME_BYTES: usize = FRAME_WIDTH as usize * FRAME_HEIGHT as usize * 4;
                const CACHE_CAP_BYTES: usize = FRAME_BYTES * 3;
                let artifact_root = report_path.parent().expect("validated Phase 0 artifact root");
                let copy_nonce = format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                );
                let copies: [PathBuf; SOURCE_COUNT] = std::array::from_fn(|index| {
                    artifact_root.join(format!("phase0-cache-pressure-{copy_nonce}-{index}.mp4"))
                });
                let result = (|| {
                    for copy in &copies {
                        fs::copy(&media, copy).map_err(|error| error.to_string())?;
                    }
                    let mut app = App::new_with_catalog_and_monitor_cache_bytes(
                        false,
                        None,
                        CACHE_CAP_BYTES,
                    );
                    app.editor.add_media_paths(copies.iter().cloned());
                    let mut video_tracks = app
                        .editor
                        .timeline
                        .tracks
                        .iter()
                        .filter(|track| track.kind == nle_timeline::TrackKind::Video)
                        .map(|track| track.id)
                        .collect::<Vec<_>>();
                    while video_tracks.len() < SOURCE_COUNT {
                        video_tracks.push(
                            app.editor
                                .timeline
                                .add_track(nle_timeline::TrackKind::Video),
                        );
                    }
                    for (track, media_id) in video_tracks
                        .into_iter()
                        .take(SOURCE_COUNT)
                        .zip(1..=SOURCE_COUNT as u32)
                    {
                        app.editor
                            .timeline
                            .insert_clip(
                                track,
                                nle_timeline::MediaId(media_id),
                                nle_timeline::Tick(0),
                                nle_timeline::Tick(2_000_000),
                                nle_timeline::Tick(0),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    app.editor.set_preview_quality(PreviewQuality::Full);
                    app.editor.set_paused_preview_quality(PreviewQuality::Full);
                    let mut preview = preview_request(&app.editor);
                    if preview.sources[..SOURCE_COUNT].iter().any(Option::is_none) {
                        return Err("four-source cache-pressure preview did not expose four visible layers".to_owned());
                    }
                    preview.output_size = [FRAME_WIDTH, FRAME_HEIGHT];
                    preview.is_scrubbing = true;
                    app.submit_monitor_decode_request(preview);

                    let deadline = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < deadline
                        && (0..SOURCE_COUNT)
                            .any(|layer| app.editor.monitor_frame_for_layer(layer).is_none())
                    {
                        app.poll_monitor_decoder();
                        thread::sleep(Duration::from_millis(5));
                    }
                    let frames = (0..SOURCE_COUNT)
                        .map(|layer| {
                            app.editor
                                .monitor_frame_for_layer(layer)
                                .ok_or_else(|| format!("cache-pressure layer {layer} did not decode before deadline"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    for (index, frame) in frames.iter().enumerate() {
                        if frame.media_id != Some(index as u32 + 1)
                            || (frame.width, frame.height) != (FRAME_WIDTH, FRAME_HEIGHT)
                        {
                            return Err(format!(
                                "cache-pressure layer {index} decoded media {:?} at {}x{}, expected media {} at {}x{}",
                                frame.media_id,
                                frame.width,
                                frame.height,
                                index + 1,
                                FRAME_WIDTH,
                                FRAME_HEIGHT
                            ));
                        }
                    }

                    let cache = app.monitor_frame_cache_pool.diagnostics();
                    let sessions = app.monitor_session_pool.diagnostics();
                    let sources = app.monitor_source_coordinator.diagnostics();
                    if cache.capacity_bytes != CACHE_CAP_BYTES
                        || cache.current_bytes != CACHE_CAP_BYTES
                        || cache.peak_bytes != CACHE_CAP_BYTES
                        || cache.eviction_count < 1
                    {
                        return Err(format!("cache-pressure decoded-frame cache diagnostics were {cache:?}, expected exact capacity {CACHE_CAP_BYTES} with at least one eviction"));
                    }
                    if sessions.active_sticky_sessions != SOURCE_COUNT
                        || sessions.active_foreground_sessions != SOURCE_COUNT
                        || sessions.active_background_sessions != 0
                        || sessions.peak_sticky_sessions < SOURCE_COUNT
                        || sessions.active_sticky_sessions > sessions.session_cap
                        || sessions.peak_sticky_sessions > sessions.session_cap
                        || sources.live_source_groups != SOURCE_COUNT
                        || sources.live_lane_actors != SOURCE_COUNT
                        || sources.retiring_lane_actors != 0
                    {
                        return Err(format!("cache-pressure source/session/actor ownership was not exactly four bounded foreground sources: sessions={sessions:?} sources={sources:?}"));
                    }

                    let mut release_preview = preview_request(&app.editor);
                    release_preview.sources = [None; MONITOR_LAYER_COUNT];
                    app.submit_monitor_decode_request(release_preview);
                    let release_deadline = Instant::now() + Duration::from_secs(5);
                    let (post_release_sessions, post_release_groups, post_release_actors) = loop {
                        app.poll_monitor_decoder();
                        let post_sessions = app.monitor_session_pool.diagnostics();
                        let post_sources = app.monitor_source_coordinator.diagnostics();
                        if post_sessions.active_sticky_sessions == 0
                            && post_sources.live_source_groups == 0
                            && post_sources.live_lane_actors + post_sources.retiring_lane_actors == 0
                        {
                            break (
                                post_sessions.active_sticky_sessions,
                                post_sources.live_source_groups,
                                post_sources.live_lane_actors + post_sources.retiring_lane_actors,
                            );
                        }
                        if Instant::now() >= release_deadline {
                            return Err(format!("cache-pressure sources did not release before deadline: sessions={post_sessions:?} sources={post_sources:?}"));
                        }
                        thread::sleep(Duration::from_millis(5));
                    };
                    let backend = app.observed_decoder_backends.first().cloned();
                    Ok((
                        backend,
                        format!(
                            "source_count={SOURCE_COUNT} frame_bytes={FRAME_BYTES} cap_bytes={CACHE_CAP_BYTES} current_bytes={} peak_bytes={} eviction_count={} peak_sessions={} session_cap={} source_groups={} source_group_cap={} lane_actors={} lane_actor_cap={} post_release_sessions={post_release_sessions} post_release_groups={post_release_groups} post_release_actors={post_release_actors}",
                            cache.current_bytes,
                            cache.peak_bytes,
                            cache.eviction_count,
                            sessions.peak_sticky_sessions,
                            sessions.session_cap,
                            sources.live_source_groups,
                            sources.source_group_cap,
                            sources.live_lane_actors,
                            sources.lane_actor_cap,
                        ),
                    ))
                })();
                for copy in &copies {
                    let _ = fs::remove_file(copy);
                }
                result
            },
        ));
        scenarios.push(phase0_run_scenario(
            "multi_source_pressure_and_idle_retirement",
            12,
            Some(Phase0BackendRole::Decoder),
            || {
                const SOURCE_COUNT: usize = 12;
                const BATCH_COUNT: usize = 3;
                const LANES_PER_BATCH: usize = SOURCE_COUNT / BATCH_COUNT;
                const FRAME_WIDTH: u32 = 160;
                const FRAME_HEIGHT: u32 = 90;
                const FRAME_BYTES: usize = FRAME_WIDTH as usize * FRAME_HEIGHT as usize * 4;
                const CACHE_CAP_BYTES: usize = FRAME_BYTES * 3;
                const SESSION_FOREGROUND_CAP: usize = 4;
                const SESSION_BACKGROUND_CAP: usize = 0;
                const SOURCE_GROUP_CAP: usize = 4;
                let artifact_root = report_path.parent().expect("validated Phase 0 artifact root");
                let copy_nonce = format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                );
                let copies: [PathBuf; SOURCE_COUNT] = std::array::from_fn(|index| {
                    artifact_root.join(format!("phase0-idle-pressure-{copy_nonce}-{index}.mp4"))
                });
                let result = (|| {
                    for copy in &copies {
                        fs::copy(&media, copy).map_err(|error| error.to_string())?;
                    }
                    let cache_pool = nle_decode::MonitorFrameCachePool::new(CACHE_CAP_BYTES);
                    let session_pool = nle_decode::MonitorSessionPool::new(
                        SESSION_FOREGROUND_CAP,
                        SESSION_BACKGROUND_CAP,
                    );
                    let source_coordinator = nle_decode::MonitorSourceCoordinator::new(
                        SOURCE_GROUP_CAP,
                        session_pool.clone(),
                    );
                    let mut observed_backend = None;
                    let mut peak_sessions = 0usize;
                    let mut peak_groups = 0usize;
                    let mut peak_actors = 0usize;
                    let mut idle_release_cycles = 0usize;
                    for batch in 0..BATCH_COUNT {
                        let decoders: Vec<nle_decode::MonitorDecoder> = (0..LANES_PER_BATCH)
                            .map(|_| {
                                nle_decode::MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
                                    || {},
                                    cache_pool.clone(),
                                    source_coordinator.clone(),
                                )
                            })
                            .collect();
                        for (lane, decoder) in decoders.iter().enumerate() {
                            let source_index = batch * LANES_PER_BATCH + lane;
                            decoder
                                .request(nle_decode::DecodeRequest {
                                    project_epoch: 1,
                                    cache_epoch: 1,
                                    request_id: (source_index + 1) as u64,
                                    media_id: (source_index + 1) as u32,
                                    path: copies[source_index].clone(),
                                    source_tick: 300_000,
                                    width: FRAME_WIDTH,
                                    height: FRAME_HEIGHT,
                                    is_scrubbing: false,
                                    prewarm_scrub_workers: false,
                                    high_quality_scaling: false,
                                    progressive_scrub_frames: false,
                                    source_frame_duration_tick: Some(33_367),
                                    acceleration: nle_decode::AccelerationPreference::Software,
                                })
                                .map_err(|error| error.to_string())?;
                        }
                        for (lane, decoder) in decoders.iter().enumerate() {
                            let source_index = batch * LANES_PER_BATCH + lane;
                            match phase0_wait_for_monitor_event(decoder, (source_index + 1) as u64)? {
                                nle_decode::DecodeEvent::Frame(frame)
                                    if frame.media_id == (source_index + 1) as u32
                                        && (frame.width, frame.height) == (FRAME_WIDTH, FRAME_HEIGHT) =>
                                {
                                    let backend = frame.backend.map(|backend| backend.display_name().to_owned()).ok_or_else(|| {
                                        format!("idle-retirement source {source_index} did not expose a decoder backend")
                                    })?;
                                    observed_backend.get_or_insert(backend);
                                }
                                nle_decode::DecodeEvent::Frame(frame) => {
                                    return Err(format!(
                                        "idle-retirement source {source_index} decoded media {} at {}x{}, expected media {} at {}x{}",
                                        frame.media_id,
                                        frame.width,
                                        frame.height,
                                        source_index + 1,
                                        FRAME_WIDTH,
                                        FRAME_HEIGHT,
                                    ));
                                }
                                nle_decode::DecodeEvent::Error(error) => {
                                    return Err(format!("idle-retirement source {source_index} failed to decode: {}", error.message));
                                }
                            }
                        }
                        let sessions = session_pool.diagnostics();
                        let sources = source_coordinator.diagnostics();
                        peak_sessions = peak_sessions.max(sessions.peak_sticky_sessions);
                        peak_groups = peak_groups.max(sources.live_source_groups);
                        peak_actors = peak_actors.max(sources.live_lane_actors);
                        if sessions.active_sticky_sessions != LANES_PER_BATCH
                            || sessions.active_foreground_sessions != LANES_PER_BATCH
                            || sessions.active_background_sessions != 0
                            || sessions.session_cap != SESSION_FOREGROUND_CAP
                            || sessions.peak_sticky_sessions > sessions.session_cap
                            || sources.live_source_groups != LANES_PER_BATCH
                            || sources.live_lane_actors != LANES_PER_BATCH
                            || sources.retiring_lane_actors != 0
                            || sources.source_group_cap != SOURCE_GROUP_CAP
                            || sources.live_source_groups > sources.source_group_cap
                            || sources.live_lane_actors + sources.retiring_lane_actors > sources.lane_actor_cap
                        {
                            return Err(format!(
                                "idle-retirement batch {batch} ownership was not exactly four bounded foreground sources: sessions={sessions:?} sources={sources:?}"
                            ));
                        }
                        for decoder in &decoders {
                            decoder.release_live_sessions().map_err(|error| error.to_string())?;
                        }
                        idle_release_cycles += 1;
                        let deadline = Instant::now() + Duration::from_secs(5);
                        loop {
                            let sessions = session_pool.diagnostics();
                            let sources = source_coordinator.diagnostics();
                            if sessions.active_sticky_sessions == 0
                                && sources.live_source_groups == 0
                                && sources.live_lane_actors == 0
                                && sources.retiring_lane_actors == 0
                            {
                                break;
                            }
                            if Instant::now() >= deadline {
                                return Err(format!(
                                    "idle-retirement batch {batch} did not reach zero ownership before deadline: sessions={sessions:?} sources={sources:?}"
                                ));
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        drop(decoders);
                    }
                    let cache = cache_pool.diagnostics();
                    let sessions = session_pool.diagnostics();
                    let sources = source_coordinator.diagnostics();
                    let minimum_evictions = (SOURCE_COUNT - 3) as u64;
                    if cache.capacity_bytes != CACHE_CAP_BYTES
                        || cache.current_bytes > CACHE_CAP_BYTES
                        || cache.peak_bytes > CACHE_CAP_BYTES
                        || cache.eviction_count < minimum_evictions
                        || peak_sessions != LANES_PER_BATCH
                        || peak_groups != LANES_PER_BATCH
                        || peak_actors != LANES_PER_BATCH
                        || sessions.active_sticky_sessions != 0
                        || sources.live_source_groups != 0
                        || sources.live_lane_actors != 0
                        || sources.retiring_lane_actors != 0
                    {
                        return Err(format!(
                            "idle-retirement final cache/ownership diagnostics were cache={cache:?} sessions={sessions:?} sources={sources:?} peaks=({peak_sessions}, {peak_groups}, {peak_actors}), expected at least {minimum_evictions} real LRU evictions and zero final ownership"
                        ));
                    }
                    Ok((
                        observed_backend,
                        format!(
                            "source_count={SOURCE_COUNT} batch_count={BATCH_COUNT} lanes_per_batch={LANES_PER_BATCH} frame_bytes={FRAME_BYTES} cache_current_bytes={} cache_peak_bytes={} cache_cap_bytes={} cache_eviction_count={} peak_sessions={peak_sessions} session_cap={} peak_source_groups={peak_groups} source_group_cap={} peak_lane_actors={peak_actors} lane_actor_cap={} idle_release_cycles={idle_release_cycles} final_sessions={} final_source_groups={} final_live_lane_actors={} final_retiring_lane_actors={}",
                            cache.current_bytes,
                            cache.peak_bytes,
                            cache.capacity_bytes,
                            cache.eviction_count,
                            sessions.session_cap,
                            sources.source_group_cap,
                            sources.lane_actor_cap,
                            sessions.active_sticky_sessions,
                            sources.live_source_groups,
                            sources.live_lane_actors,
                            sources.retiring_lane_actors,
                        ),
                    ))
                })();
                for copy in &copies {
                    let _ = fs::remove_file(copy);
                }
                result
            },
        ));
        scenarios.push(phase0_run_scenario("ffmpeg_export_cancellation", 1, Some(Phase0BackendRole::Encoder), || {
            for artifact in phase0_export_artifacts(&output) {
                let _ = fs::remove_file(artifact);
            }
            let result = (|| {
                let ffmpeg = ffmpeg_root
                    .join("bin/ffmpeg.exe")
                    .canonicalize()
                    .map_err(|error| error.to_string())?;
                if !ffmpeg.is_absolute() || !ffmpeg.is_file() {
                    return Err("validated FFmpeg executable is not an absolute file".to_owned());
                }
                let mut app = App::new_with_catalog(false, None);
                app.screen = Screen::Editor;
                app.editor.add_media_paths([media.clone()]);
                let track = app
                    .editor
                    .timeline
                    .tracks
                    .iter()
                    .find(|track| track.kind == nle_timeline::TrackKind::Video)
                    .ok_or_else(|| "missing default video track".to_owned())?
                    .id;
                app.editor
                    .timeline
                    .insert_clip(
                        track,
                        nle_timeline::MediaId(1),
                        nle_timeline::Tick(0),
                        nle_timeline::Tick(2_000_000),
                        nle_timeline::Tick(0),
                    )
                    .map_err(|error| error.to_string())?;
                app.start_video_export_with_ffmpeg(output.clone(), ffmpeg);
                let deadline = Instant::now() + Duration::from_secs(8);
                let encoder = loop {
                    if Instant::now() >= deadline {
                        return Err("FFmpeg export never emitted EncoderStarted before deadline".to_owned());
                    }
                    let job = app
                        .export_job
                        .as_ref()
                        .ok_or_else(|| "FFmpeg export did not start".to_owned())?;
                    match job.try_recv() {
                        Ok(nle_export::ExportEvent::EncoderStarted(encoder)) => break encoder,
                        Ok(nle_export::ExportEvent::Failed(error)) => {
                            return Err(format!("FFmpeg export failed before cancellation: {error}"));
                        }
                        Ok(nle_export::ExportEvent::Cancelled) => {
                            return Err("FFmpeg export cancelled before EncoderStarted was observed".to_owned());
                        }
                        Ok(nle_export::ExportEvent::Completed(_)) => {
                            return Err("two-second export completed before cancellation could be issued".to_owned());
                        }
                        Ok(nle_export::ExportEvent::Progress(_))
                        | Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            return Err("FFmpeg export event channel disconnected before cancellation".to_owned());
                        }
                    }
                };
                app.export_job
                    .as_ref()
                    .expect("job remains live after EncoderStarted")
                    .cancel();
                let deadline = Instant::now() + Duration::from_secs(8);
                while app.export_job.is_some() && Instant::now() < deadline {
                    app.poll_video_export();
                    thread::sleep(Duration::from_millis(5));
                }
                if app.export_job.is_some() {
                    return Err("cancelled FFmpeg export worker did not terminate before deadline".to_owned());
                }
                if !matches!(app.editor.export_status, nle_ui_core::EditorExportStatus::Idle) {
                    return Err("cancelled FFmpeg export did not publish the terminal Cancelled state".to_owned());
                }
                let residue = phase0_export_artifacts(&output);
                if !residue.is_empty() {
                    return Err(format!("cancelled FFmpeg export left final, staged, or filter artifacts: {residue:?}"));
                }
                let mut encoder_backend = None;
                observe_encoder_backend(&mut encoder_backend, encoder);
                Ok((encoder_backend, "an explicit absolute bundled FFmpeg executable emitted EncoderStarted, then cancellation reached the worker terminal state with no final, staged, or filter artifact".to_owned()))
            })();
            for artifact in phase0_export_artifacts(&output) {
                let _ = fs::remove_file(artifact);
            }
            result
        }));

        let passed = scenarios.iter().all(|scenario| scenario.passed);
        let report = Phase0Report {
            schema_version: 4,
            status: if passed { "passed" } else { "failed" },
            scenario_count: scenarios.len(),
            scenarios,
        };
        phase0_write_report(&report_path, &report)
            .expect("atomically write Phase 0 scenario report");
        assert!(
            passed,
            "Phase 0 scenario matrix failed; inspect {}",
            report_path.display()
        );
    }
}
