//! Retained four-layer GPU compositor for the project monitor.

use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Instant,
};

use bytemuck::{Pod, Zeroable};
use nle_compositor::{CompositeQuad, MAX_COMPOSITE_LAYERS, PixelSize, Uv};

const VIEWER_SHADER: &str = include_str!("viewer_compositor.wgsl");
const VERTICES_PER_LAYER: usize = 6;
const COMPOSITOR_TIMING_SAMPLE_COUNT: usize = 120;
const TIMESTAMP_QUERY_COUNT: u32 = 2;
const TIMESTAMP_RESULT_BYTES: u64 = 16;
const TIMESTAMP_RESOLVE_BYTES: u64 = 256;
/// Free GPU resources retained across source/canvas dimension changes. This is deliberately
/// below one typical 4K layer pair, so resize churn cannot pin a large amount of VRAM.
const RESOURCE_POOL_BYTE_CAP: u64 = 32 * 1024 * 1024;
const MAX_POOLED_LAYER_BUNDLES: usize = MAX_COMPOSITE_LAYERS;
const MAX_POOLED_OUTPUT_PAIRS: usize = 1;

/// Isolated GPU execution time for the viewer compositor compose pass.
///
/// `supported` reports whether the current device enabled timestamp queries;
/// it is intentionally independent of whether any changed composition has yet
/// produced a sample.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewerCompositorGpuTiming {
    pub supported: bool,
    pub samples: usize,
    pub p95_ms: f32,
    pub max_ms: f32,
}

/// Sampling used when a retained viewer layer is scaled during composition.
///
/// Bicubic is the default for monitor presentation. It is implemented in WGSL rather than
/// relying on a WebGPU sampler mode, which only exposes nearest and linear filtering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewerSamplingQuality {
    Nearest,
    Bilinear,
    #[default]
    Bicubic,
}

impl ViewerSamplingQuality {
    const fn shader_value(self) -> u32 {
        match self {
            Self::Nearest => 0,
            Self::Bilinear => 1,
            Self::Bicubic => 2,
        }
    }
}

/// Retained command-encoding evidence for the project-monitor presentation path.
///
/// An upload serial identifies a successfully written layer texture. A painted serial is the
/// upload identity captured by the most recent canvas blit. This reports command encoding and
/// submission only; it does not establish GPU completion or display scanout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewerPresentationEvidence {
    pub upload_serials: [u64; MAX_COMPOSITE_LAYERS],
    pub painted_upload_serials: [Option<u64>; MAX_COMPOSITE_LAYERS],
    pub paint_serial: u64,
}

#[derive(Default)]
struct ViewerPresentationTracker {
    next_upload_serial: u64,
    composed_upload_serials: [Option<u64>; MAX_COMPOSITE_LAYERS],
    upload_serials: [AtomicU64; MAX_COMPOSITE_LAYERS],
    painted_upload_serials: [AtomicU64; MAX_COMPOSITE_LAYERS],
    paint_serial: AtomicU64,
}

impl ViewerPresentationTracker {
    fn record_upload(&mut self, layer: usize) {
        self.next_upload_serial = self
            .next_upload_serial
            .checked_add(1)
            .expect("viewer upload serial exhausted");
        self.upload_serials[layer].store(self.next_upload_serial, Ordering::Relaxed);
    }

    fn clear_layer(&mut self, layer: usize) {
        self.upload_serials[layer].store(0, Ordering::Relaxed);
        self.composed_upload_serials[layer] = None;
    }

    fn clear(&mut self) {
        for serial in &self.upload_serials {
            serial.store(0, Ordering::Relaxed);
        }
        self.composed_upload_serials = [None; MAX_COMPOSITE_LAYERS];
    }

    fn capture_composition(&mut self, rendered: [bool; MAX_COMPOSITE_LAYERS]) {
        self.composed_upload_serials = std::array::from_fn(|layer| {
            rendered[layer]
                .then(|| self.upload_serials[layer].load(Ordering::Relaxed))
                .filter(|serial| *serial != 0)
        });
    }

    fn record_paint(&self) {
        for (painted, serial) in self
            .painted_upload_serials
            .iter()
            .zip(self.composed_upload_serials)
        {
            painted.store(serial.unwrap_or_default(), Ordering::Relaxed);
        }
        self.paint_serial.fetch_add(1, Ordering::Relaxed);
    }

    fn evidence(&self) -> ViewerPresentationEvidence {
        ViewerPresentationEvidence {
            upload_serials: std::array::from_fn(|layer| {
                self.upload_serials[layer].load(Ordering::Relaxed)
            }),
            painted_upload_serials: std::array::from_fn(|layer| {
                let serial = self.painted_upload_serials[layer].load(Ordering::Relaxed);
                (serial != 0).then_some(serial)
            }),
            paint_serial: self.paint_serial.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct GpuTimestampWindow {
    samples: [f32; COMPOSITOR_TIMING_SAMPLE_COUNT],
    len: usize,
    next: usize,
}

impl Default for GpuTimestampWindow {
    fn default() -> Self {
        Self {
            samples: [0.0; COMPOSITOR_TIMING_SAMPLE_COUNT],
            len: 0,
            next: 0,
        }
    }
}

impl GpuTimestampWindow {
    fn push(&mut self, milliseconds: f32) {
        self.samples[self.next] = milliseconds;
        self.next = (self.next + 1) % COMPOSITOR_TIMING_SAMPLE_COUNT;
        self.len = (self.len + 1).min(COMPOSITOR_TIMING_SAMPLE_COUNT);
    }

    fn snapshot(&self, supported: bool) -> ViewerCompositorGpuTiming {
        if self.len == 0 {
            return ViewerCompositorGpuTiming {
                supported,
                ..Default::default()
            };
        }
        let mut ordered = self.samples;
        ordered[..self.len].sort_unstable_by(f32::total_cmp);
        let p95_index = self.len.saturating_mul(95).div_ceil(100).saturating_sub(1);
        ViewerCompositorGpuTiming {
            supported,
            samples: self.len,
            p95_ms: ordered[p95_index],
            max_ms: ordered[self.len - 1],
        }
    }
}

/// A mapped readback is exclusively owned by the render thread. The callback
/// only changes this state, keeping device polling inexpensive.
#[derive(Debug, Default)]
struct GpuTimestampMailbox {
    // 0 = idle, 1 = mapping scheduled, 2 = mapped readback, 3 = map failed.
    state: AtomicU8,
}

impl GpuTimestampMailbox {
    fn reserve(&self) -> bool {
        self.state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn complete(&self, success: bool) {
        self.state
            .store(if success { 2 } else { 3 }, Ordering::Release);
    }

    fn pending(&self) -> bool {
        self.state.load(Ordering::Acquire) != 0
    }

    fn take_completed(&self) -> Option<bool> {
        match self
            .state
            .compare_exchange(2, 0, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(true),
            Err(3) => self
                .state
                .compare_exchange(3, 0, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| false),
            Err(_) => None,
        }
    }
}

struct GpuTimestampResources {
    query_set: wgpu::QuerySet,
    resolve_buffer: wgpu::Buffer,
    readback_buffer: wgpu::Buffer,
    mailbox: Arc<GpuTimestampMailbox>,
    timing: GpuTimestampWindow,
}

fn timestamp_sample_ms(bytes: &[u8], timestamp_period_ns: f32) -> Option<f32> {
    let start = u64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?);
    let end = u64::from_ne_bytes(
        bytes
            .get(8..TIMESTAMP_RESULT_BYTES as usize)?
            .try_into()
            .ok()?,
    );
    let ticks = end.checked_sub(start)?;
    if ticks == 0 || !timestamp_period_ns.is_finite() || timestamp_period_ns <= 0.0 {
        return None;
    }
    let milliseconds = ticks as f64 * timestamp_period_ns as f64 / 1_000_000.0;
    (milliseconds.is_finite() && milliseconds > 0.0 && milliseconds <= f32::MAX as f64)
        .then_some(milliseconds as f32)
}

/// CPU time spent encoding a changed project-monitor composition.
///
/// This deliberately excludes GPU execution, GPU completion, and presentation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewerCompositorEncodeTiming {
    pub samples: usize,
    pub p95_ms: f32,
    pub max_ms: f32,
}

/// Maximum number of ordered encoded-sRGB color-correction nodes per monitor layer.
pub const MAX_COLOR_CORRECTIONS_PER_LAYER: usize = 8;
/// One lookup entry per encoded 8-bit channel value, matching export precision.
pub const COLOR_CURVE_LUT_SAMPLES: usize = 256;
pub const MAX_COLOR_CURVE_POINTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewerColorCurve {
    pub points: [[f32; 2]; MAX_COLOR_CURVE_POINTS],
    pub count: u32,
}

impl Default for ViewerColorCurve {
    fn default() -> Self {
        let mut points = [[0.0; 2]; MAX_COLOR_CURVE_POINTS];
        points[1] = [1.0, 1.0];
        Self { points, count: 2 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewerRgbCurves {
    pub master: ViewerColorCurve,
    pub red: ViewerColorCurve,
    pub green: ViewerColorCurve,
    pub blue: ViewerColorCurve,
}

/// The native viewer operation represented by a fixed correction-payload slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewerColorCorrectionOperation {
    #[default]
    BasicCorrection,
    Vignette,
}

/// One ordered encoded-sRGB effect node for the project monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewerColorCorrection {
    pub operation: ViewerColorCorrectionOperation,
    pub temperature: f32,
    pub tint: f32,
    pub saturation: f32,
    pub exposure: f32,
    /// Encoded-sRGB brightness offset, clamped to -1.0..=1.0 before GPU upload.
    pub brightness: f32,
    /// Encoded-sRGB contrast multiplier, clamped to 0.0..=4.0 before GPU upload.
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    /// Near-white tonal adjustment, clamped to -1.0..=1.0 before GPU upload.
    pub whites: f32,
    /// Near-black tonal adjustment, clamped to -1.0..=1.0 before GPU upload.
    pub blacks: f32,
    /// Vignette amount, clamped to 0.0..=1.0 before GPU upload.
    pub vignette_amount: f32,
    /// Vignette radius where darkening starts, clamped to 0.0..=1.0 before GPU upload.
    pub vignette_midpoint: f32,
    /// Vignette soft-edge width, clamped to 0.0..=1.0 before GPU upload.
    pub vignette_feather: f32,
    /// Vignette center offset in normalized image coordinates, clamped to -1.0..=1.0.
    pub vignette_center_x: f32,
    /// Vignette center offset in normalized image coordinates, clamped to -1.0..=1.0.
    pub vignette_center_y: f32,
    /// Static control points expanded into a full 8-bit LUT only during GPU upload.
    pub curves: ViewerRgbCurves,
}

impl Default for ViewerColorCorrection {
    fn default() -> Self {
        Self {
            operation: ViewerColorCorrectionOperation::BasicCorrection,
            temperature: 0.0,
            tint: 0.0,
            saturation: 1.0,
            exposure: 0.0,
            brightness: 0.0,
            contrast: 1.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            vignette_amount: 0.0,
            vignette_midpoint: 0.5,
            vignette_feather: 0.0,
            vignette_center_x: 0.0,
            vignette_center_y: 0.0,
            curves: ViewerRgbCurves::default(),
        }
    }
}

impl ViewerColorCorrection {
    pub fn vignette(
        amount: f32,
        midpoint: f32,
        feather: f32,
        center_x: f32,
        center_y: f32,
    ) -> Self {
        Self {
            operation: ViewerColorCorrectionOperation::Vignette,
            vignette_amount: amount,
            vignette_midpoint: midpoint,
            vignette_feather: feather,
            vignette_center_x: center_x,
            vignette_center_y: center_y,
            ..Self::default()
        }
    }
}

/// One decoded frame paired with the shared compositor geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewerLayerPrimitive {
    pub quad: CompositeQuad,
    /// Decoded-content UV corners in TL, TR, BR, BL order.
    pub content_uv: [Uv; 4],
    /// Ordered encoded-sRGB corrections. Only the leading active count is applied.
    pub color_corrections: [ViewerColorCorrection; MAX_COLOR_CORRECTIONS_PER_LAYER],
    /// Active correction count. Safely clamped to the fixed GPU payload capacity.
    pub color_correction_count: u32,
}

/// One retained project-monitor frame. Layer slots are bottom-to-top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewerFrame {
    pub project_size: PixelSize,
    pub logical_canvas_rect: egui::Rect,
    /// Full-project black mattes inserted before each media-layer boundary, bottom-to-top.
    pub black_mattes_before: [f32; MAX_COMPOSITE_LAYERS + 1],
    /// Full-project white mattes inserted before each media-layer boundary, bottom-to-top.
    pub white_mattes_before: [f32; MAX_COMPOSITE_LAYERS + 1],
    pub layers: [Option<ViewerLayerPrimitive>; MAX_COMPOSITE_LAYERS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerUploadError {
    LayerOutOfBounds { layer: usize },
    ZeroDimension,
    InvalidRgbaLength { expected: usize, actual: usize },
}

impl fmt::Display for ViewerUploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LayerOutOfBounds { layer } => write!(
                f,
                "viewer layer {layer} is outside 0..{MAX_COMPOSITE_LAYERS}"
            ),
            Self::ZeroDimension => write!(f, "viewer texture dimensions must both be non-zero"),
            Self::InvalidRgbaLength { expected, actual } => write!(
                f,
                "viewer RGBA byte length must be {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ViewerUploadError {}

struct CallbackState {
    frame: Option<ViewerFrame>,
    generation: u64,
    compositor_encode_ms: [f32; COMPOSITOR_TIMING_SAMPLE_COUNT],
    compositor_encode_count: usize,
    compositor_encode_next: usize,
}

impl Default for CallbackState {
    fn default() -> Self {
        Self {
            frame: None,
            generation: 0,
            compositor_encode_ms: [0.0; COMPOSITOR_TIMING_SAMPLE_COUNT],
            compositor_encode_count: 0,
            compositor_encode_next: 0,
        }
    }
}

impl CallbackState {
    fn record_compositor_encode(&mut self, duration: std::time::Duration) {
        self.compositor_encode_ms[self.compositor_encode_next] = duration.as_secs_f32() * 1_000.0;
        self.compositor_encode_next =
            (self.compositor_encode_next + 1) % COMPOSITOR_TIMING_SAMPLE_COUNT;
        self.compositor_encode_count =
            (self.compositor_encode_count + 1).min(COMPOSITOR_TIMING_SAMPLE_COUNT);
    }

    fn compositor_encode_timing(&self) -> ViewerCompositorEncodeTiming {
        let mut ordered = self.compositor_encode_ms;
        ordered[..self.compositor_encode_count].sort_unstable_by(f32::total_cmp);
        let p95_index = self
            .compositor_encode_count
            .saturating_mul(95)
            .div_ceil(100)
            .saturating_sub(1);
        ViewerCompositorEncodeTiming {
            samples: self.compositor_encode_count,
            p95_ms: ordered.get(p95_index).copied().unwrap_or_default(),
            max_ms: ordered
                .get(self.compositor_encode_count.saturating_sub(1))
                .copied()
                .unwrap_or_default(),
        }
    }
}

/// Thread-safe retained frame input for the project-monitor paint callback.
#[derive(Clone, Default)]
pub struct ViewerCompositorCallbackHandle {
    state: Arc<Mutex<CallbackState>>,
}

impl ViewerCompositorCallbackHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_frame(&self, frame: ViewerFrame) {
        let mut state = self.state.lock().expect("viewer compositor lock");
        if state.frame != Some(frame) {
            state.frame = Some(frame);
            state.generation = state.generation.wrapping_add(1);
        }
    }

    pub fn clear(&self) {
        let mut state = self.state.lock().expect("viewer compositor lock");
        if state.frame.take().is_some() {
            state.generation = state.generation.wrapping_add(1);
        }
    }

    pub fn compositor_encode_timing(&self) -> ViewerCompositorEncodeTiming {
        self.state
            .lock()
            .expect("viewer compositor lock")
            .compositor_encode_timing()
    }

    /// Returns the latest bounded timing snapshot without ever waiting for the render callback.
    ///
    /// The live HUD uses this path so diagnostics cannot introduce a blocking renderer lock on
    /// the UI thread. Qualification reports may still use [`Self::compositor_encode_timing`]
    /// after the measured surface work has completed.
    pub fn try_compositor_encode_timing(&self) -> Option<ViewerCompositorEncodeTiming> {
        self.state
            .try_lock()
            .ok()
            .map(|state| state.compositor_encode_timing())
    }

    /// Adds the monitor callback at its current painter position.
    pub fn install(&self, painter: &egui::Painter, rect: egui::Rect) {
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            ViewerCompositorCallback {
                handle: self.clone(),
                logical_canvas_rect: rect,
            },
        ));
    }

    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.state
            .lock()
            .expect("viewer compositor lock")
            .generation
    }
}

#[derive(Clone)]
struct ViewerCompositorCallback {
    handle: ViewerCompositorCallbackHandle,
    logical_canvas_rect: egui::Rect,
}

impl egui_wgpu::CallbackTrait for ViewerCompositorCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(renderer) = resources.get_mut::<ViewerCompositorRenderer>() else {
            return Vec::new();
        };
        let mut state = self.handle.state.lock().expect("viewer compositor lock");
        let started_at = Instant::now();
        if renderer.prepare(
            device,
            queue,
            encoder,
            state.frame,
            state.generation,
            physical_canvas_size(
                self.logical_canvas_rect,
                screen.pixels_per_point,
                screen.size_in_pixels,
            ),
        ) {
            state.record_compositor_encode(started_at.elapsed());
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(renderer) = resources.get::<ViewerCompositorRenderer>() else {
            return;
        };
        renderer.paint(pass, info);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ViewerVertex {
    position: [f32; 2],
    uv: [f32; 2],
    opacity: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuColorCorrection {
    /// temperature, tint, saturation, exposure
    color: [f32; 4],
    /// brightness, contrast, highlights, shadows
    light: [f32; 4],
    /// operation (0 = basic, 1 = vignette), amount, midpoint, feather
    effect: [f32; 4],
    /// Vignette center x, center y, or Basic Correction whites, blacks.
    center: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuColorCorrectionStack {
    corrections: [GpuColorCorrection; MAX_COLOR_CORRECTIONS_PER_LAYER],
    count: u32,
    sampling_quality: u32,
    _padding: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuCurveLutStack {
    /// Node-major final RGB lookup values. Curves live in a storage buffer because the complete
    /// eight-node 8-bit payload is larger than WebGPU's guaranteed 16 KiB uniform limit.
    samples: [[f32; 4]; MAX_COLOR_CORRECTIONS_PER_LAYER * COLOR_CURVE_LUT_SAMPLES],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MatteVertex {
    opacity: f32,
    color: [f32; 3],
}

struct LayerTexture {
    _texture: wgpu::Texture,
    nearest_bind_group: wgpu::BindGroup,
    bilinear_bind_group: wgpu::BindGroup,
    premultiply_bind_group: wgpu::BindGroup,
    premultiplied_view: wgpu::TextureView,
    _premultiplied_texture: wgpu::Texture,
    correction_buffer: wgpu::Buffer,
    curve_buffer: wgpu::Buffer,
    premultiply_needed: bool,
}
struct OutputTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    blit_bind_group: wgpu::BindGroup,
    _correction_buffer: wgpu::Buffer,
    _curve_buffer: wgpu::Buffer,
}

struct PooledLayerTexture {
    size: PixelSize,
    bytes: u64,
    order: u64,
    texture: LayerTexture,
}

struct PooledOutputTargets {
    size: PixelSize,
    bytes: u64,
    order: u64,
    outputs: [OutputTarget; 2],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ViewerCompositorPoolCounters {
    layer_reuses: u64,
    output_reuses: u64,
    layer_allocations: u64,
    output_allocations: u64,
    rejected_oversize: u64,
    evictions: u64,
}

/// GPU resources for a fixed four-layer project-monitor composition.
pub struct ViewerCompositorRenderer {
    premultiply_pipeline: wgpu::RenderPipeline,
    compose_pipeline: wgpu::RenderPipeline,
    matte_pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    premultiply_layout: wgpu::BindGroupLayout,
    nearest_sampler: wgpu::Sampler,
    bilinear_sampler: wgpu::Sampler,
    vertex_buffer: wgpu::Buffer,
    matte_vertex_buffer: wgpu::Buffer,
    layers: [Option<LayerTexture>; MAX_COMPOSITE_LAYERS],
    layer_sizes: [Option<PixelSize>; MAX_COMPOSITE_LAYERS],
    layer_pool: [Option<PooledLayerTexture>; MAX_POOLED_LAYER_BUNDLES],
    output_pool: Option<PooledOutputTargets>,
    pooled_bytes: u64,
    next_pool_order: u64,
    pool_counters: ViewerCompositorPoolCounters,
    outputs: Option<[OutputTarget; 2]>,
    output_size: Option<PixelSize>,
    front_output: usize,
    // Fixed scratch avoids per-frame heap allocations. The compositor encodes into the
    // callback-provided encoder, so it owns no command buffers to pool.
    scratch_vertices: [ViewerVertex; MAX_COMPOSITE_LAYERS * VERTICES_PER_LAYER],
    scratch_mattes: [MatteVertex; MAX_COMPOSITE_LAYERS + 1],
    scratch_layer_counts: [u32; MAX_COMPOSITE_LAYERS],
    input_generation: u64,
    applied_input_generation: u64,
    applied_frame_generation: u64,
    sampling_quality: ViewerSamplingQuality,
    presentation: ViewerPresentationTracker,
    gpu_timing_enabled: bool,
    gpu_timestamps: Option<GpuTimestampResources>,
}

impl ViewerCompositorRenderer {
    pub fn new(device: &wgpu::Device, presentation_format: wgpu::TextureFormat) -> Self {
        let texture_layout = texture_layout(device);
        let premultiply_layout = premultiply_layout(device);
        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewer compositor nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let bilinear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewer compositor bilinear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewer compositor shader"),
            source: wgpu::ShaderSource::Wgsl(VIEWER_SHADER.into()),
        });
        let compose_pipeline = compose_pipeline(
            device,
            &shader,
            &texture_layout,
            wgpu::TextureFormat::Rgba8Unorm,
            "compose",
        );
        let premultiply_pipeline = premultiply_pipeline(
            device,
            &shader,
            &premultiply_layout,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let matte_pipeline =
            matte_pipeline(device, &shader, wgpu::TextureFormat::Rgba8Unorm, "matte");
        let blit_pipeline = blit_pipeline(
            device,
            &shader,
            &texture_layout,
            presentation_format,
            "blit",
        );
        Self {
            premultiply_pipeline,
            compose_pipeline,
            matte_pipeline,
            blit_pipeline,
            texture_layout,
            premultiply_layout,
            nearest_sampler,
            bilinear_sampler,
            vertex_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewer compositor vertices"),
                size: (MAX_COMPOSITE_LAYERS
                    * VERTICES_PER_LAYER
                    * std::mem::size_of::<ViewerVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            matte_vertex_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewer compositor matte vertices"),
                size: ((MAX_COMPOSITE_LAYERS + 1) * std::mem::size_of::<MatteVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            layers: std::array::from_fn(|_| None),
            layer_sizes: [None; MAX_COMPOSITE_LAYERS],
            layer_pool: std::array::from_fn(|_| None),
            output_pool: None,
            pooled_bytes: 0,
            next_pool_order: 0,
            pool_counters: ViewerCompositorPoolCounters::default(),
            outputs: None,
            output_size: None,
            front_output: 0,
            scratch_vertices: [ViewerVertex::zeroed(); MAX_COMPOSITE_LAYERS * VERTICES_PER_LAYER],
            scratch_mattes: [MatteVertex::zeroed(); MAX_COMPOSITE_LAYERS + 1],
            scratch_layer_counts: [0; MAX_COMPOSITE_LAYERS],
            input_generation: 0,
            applied_input_generation: u64::MAX,
            applied_frame_generation: u64::MAX,
            sampling_quality: ViewerSamplingQuality::default(),
            presentation: ViewerPresentationTracker::default(),
            gpu_timing_enabled: false,
            gpu_timestamps: device
                .features()
                .contains(wgpu::Features::TIMESTAMP_QUERY)
                .then(|| GpuTimestampResources {
                    query_set: device.create_query_set(&wgpu::QuerySetDescriptor {
                        label: Some("viewer compositor timestamps"),
                        ty: wgpu::QueryType::Timestamp,
                        count: TIMESTAMP_QUERY_COUNT,
                    }),
                    // Query resolves require a 256-byte-aligned destination.
                    resolve_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("viewer compositor timestamp resolve"),
                        size: TIMESTAMP_RESOLVE_BYTES,
                        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    }),
                    readback_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("viewer compositor timestamp readback"),
                        size: TIMESTAMP_RESULT_BYTES,
                        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }),
                    mailbox: Arc::new(GpuTimestampMailbox::default()),
                    timing: GpuTimestampWindow::default(),
                }),
        }
    }

    pub fn set_gpu_timing_enabled(&mut self, enabled: bool) {
        self.gpu_timing_enabled = enabled;
    }

    pub fn gpu_timing(&self) -> ViewerCompositorGpuTiming {
        match &self.gpu_timestamps {
            Some(timestamps) => timestamps.timing.snapshot(true),
            None => ViewerCompositorGpuTiming::default(),
        }
    }

    /// Changes retained-layer sampling and schedules a composition without requiring a decode
    /// upload or creating a shader/pipeline during playback.
    pub fn set_sampling_quality(&mut self, sampling_quality: ViewerSamplingQuality) {
        if self.sampling_quality != sampling_quality {
            self.sampling_quality = sampling_quality;
            self.input_generation = self.input_generation.wrapping_add(1);
        }
    }

    pub fn sampling_quality(&self) -> ViewerSamplingQuality {
        self.sampling_quality
    }

    pub fn gpu_timing_pending(&self) -> bool {
        self.gpu_timestamps
            .as_ref()
            .is_some_and(|timestamps| timestamps.mailbox.pending())
    }

    pub fn presentation_evidence(&self) -> ViewerPresentationEvidence {
        self.presentation.evidence()
    }

    /// Drains a previously mapped pass timestamp after a nonblocking device poll.
    pub fn drain_gpu_timing(&mut self, queue: &wgpu::Queue) {
        let Some(timestamps) = &mut self.gpu_timestamps else {
            return;
        };
        let Some(mapped) = timestamps.mailbox.take_completed() else {
            return;
        };
        if !mapped {
            return;
        }
        let bytes = timestamps.readback_buffer.slice(..).get_mapped_range();
        let sample = timestamp_sample_ms(&bytes, queue.get_timestamp_period());
        drop(bytes);
        timestamps.readback_buffer.unmap();
        if let Some(milliseconds) = sample {
            timestamps.timing.push(milliseconds);
        }
    }

    pub fn upload_layer_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layer: usize,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), ViewerUploadError> {
        validate_upload(layer, width, height, rgba.len())?;
        let size = PixelSize::new(width, height);
        if self.layer_sizes[layer] != Some(size) {
            if let (Some(texture), Some(previous_size)) =
                (self.layers[layer].take(), self.layer_sizes[layer])
            {
                self.pool_layer(previous_size, texture);
            }
            self.layers[layer] = Some(self.take_layer(size).unwrap_or_else(|| {
                self.pool_counters.layer_allocations += 1;
                create_layer_texture(
                    device,
                    &self.texture_layout,
                    &self.premultiply_layout,
                    &self.nearest_sampler,
                    &self.bilinear_sampler,
                    width,
                    height,
                )
            }));
            self.layer_sizes[layer] = Some(size);
        }
        let texture = self.layers[layer].as_ref().expect("viewer layer allocated");
        // The retained texture is intentionally the only upload target for this slot.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture._texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.layers[layer]
            .as_mut()
            .expect("viewer layer allocated")
            .premultiply_needed = true;
        self.input_generation = self.input_generation.wrapping_add(1);
        self.presentation.record_upload(layer);
        Ok(())
    }

    pub fn clear_layer(&mut self, layer: usize) -> Result<(), ViewerUploadError> {
        validate_layer(layer)?;
        if let Some(texture) = self.layers[layer].take() {
            if let Some(size) = self.layer_sizes[layer] {
                self.pool_layer(size, texture);
            }
            self.input_generation = self.input_generation.wrapping_add(1);
        }
        self.layer_sizes[layer] = None;
        self.presentation.clear_layer(layer);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.layers = std::array::from_fn(|_| None);
        self.outputs = None;
        self.layer_pool = std::array::from_fn(|_| None);
        self.output_pool = None;
        self.pooled_bytes = 0;
        self.output_size = None;
        self.layer_sizes = [None; MAX_COMPOSITE_LAYERS];
        self.input_generation = self.input_generation.wrapping_add(1);
        self.applied_input_generation = u64::MAX;
        self.presentation.clear();
    }

    /// Test-only headless bridge for app-level GPU qualifications. The caller
    /// owns submission and readback; production presentation remains private.
    #[cfg(feature = "qualification")]
    #[doc(hidden)]
    pub fn qualification_prepare<'a>(
        &'a mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frame: ViewerFrame,
        frame_generation: u64,
        canvas_size: PixelSize,
    ) -> Option<&'a wgpu::Texture> {
        self.prepare(
            device,
            queue,
            encoder,
            Some(frame),
            frame_generation,
            canvas_size,
        )
        .then(|| {
            &self
                .outputs
                .as_ref()
                .expect("qualification output prepared")[self.front_output]
                ._texture
        })
    }

    #[cfg(feature = "qualification")]
    #[doc(hidden)]
    pub fn qualification_composed_upload_serials(&self) -> [Option<u64>; MAX_COMPOSITE_LAYERS] {
        self.presentation.composed_upload_serials
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frame: Option<ViewerFrame>,
        frame_generation: u64,
        canvas_size: PixelSize,
    ) -> bool {
        let Some(frame) = frame.filter(|frame| frame.project_size.is_nonzero()) else {
            if let (Some(outputs), Some(size)) = (self.outputs.take(), self.output_size) {
                self.pool_outputs(size, outputs);
            }
            self.output_size = None;
            self.presentation.composed_upload_serials = [None; MAX_COMPOSITE_LAYERS];
            return false;
        };
        let resized = self.ensure_outputs(device, canvas_size);
        if !composition_required(
            resized,
            self.applied_frame_generation,
            frame_generation,
            self.applied_input_generation,
            self.input_generation,
        ) {
            return false;
        }
        for texture in self.layers.iter_mut().flatten() {
            if !texture.premultiply_needed {
                continue;
            }
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewer compositor premultiply pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture.premultiplied_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.premultiply_pipeline);
            pass.set_bind_group(0, &texture.premultiply_bind_group, &[]);
            pass.draw(0..VERTICES_PER_LAYER as u32, 0..1);
            texture.premultiply_needed = false;
        }
        let back = 1 - self.front_output;
        self.scratch_vertices = [ViewerVertex::zeroed(); MAX_COMPOSITE_LAYERS * VERTICES_PER_LAYER];
        self.scratch_mattes = matte_vertices(frame.black_mattes_before, frame.white_mattes_before);
        self.scratch_layer_counts = [0; MAX_COMPOSITE_LAYERS];
        for (layer, primitive) in frame.layers.iter().enumerate() {
            if primitive.is_some() && self.layers[layer].is_some() {
                let source = primitive.expect("checked");
                write_vertices(
                    &mut self.scratch_vertices
                        [layer * VERTICES_PER_LAYER..(layer + 1) * VERTICES_PER_LAYER],
                    source,
                    frame.project_size,
                );
                let mut correction_stack = gpu_color_correction_stack(source);
                correction_stack.sampling_quality = self.sampling_quality.shader_value();
                let texture = self.layers[layer].as_ref().expect("checked layer texture");
                queue.write_buffer(
                    &texture.correction_buffer,
                    0,
                    bytemuck::bytes_of(&correction_stack),
                );
                let curve_stack = gpu_curve_lut_stack(source);
                queue.write_buffer(&texture.curve_buffer, 0, bytemuck::bytes_of(&curve_stack));
                self.scratch_layer_counts[layer] = VERTICES_PER_LAYER as u32;
            }
        }
        self.presentation
            .capture_composition(std::array::from_fn(|layer| {
                self.scratch_layer_counts[layer] != 0
            }));
        queue.write_buffer(
            &self.vertex_buffer,
            0,
            bytemuck::cast_slice(&self.scratch_vertices),
        );
        queue.write_buffer(
            &self.matte_vertex_buffer,
            0,
            bytemuck::cast_slice(&self.scratch_mattes),
        );
        let outputs = self.outputs.as_ref().expect("output targets allocated");
        let record_gpu_timing = self.gpu_timing_enabled
            && self
                .gpu_timestamps
                .as_ref()
                .is_some_and(|timestamps| timestamps.mailbox.reserve());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("viewer compositor compose pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &outputs[back].view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: record_gpu_timing.then(|| {
                let timestamps = self
                    .gpu_timestamps
                    .as_ref()
                    .expect("timing resources reserved");
                wgpu::RenderPassTimestampWrites {
                    query_set: &timestamps.query_set,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }
            }),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.matte_pipeline);
        pass.set_vertex_buffer(0, self.matte_vertex_buffer.slice(..));
        pass.draw(0..VERTICES_PER_LAYER as u32, 0..1);
        pass.set_pipeline(&self.compose_pipeline);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        for (layer, &count) in self.scratch_layer_counts.iter().enumerate() {
            if count == 0 {
                pass.set_pipeline(&self.matte_pipeline);
                pass.set_vertex_buffer(0, self.matte_vertex_buffer.slice(..));
                pass.draw(
                    0..VERTICES_PER_LAYER as u32,
                    (layer + 1) as u32..(layer + 2) as u32,
                );
                pass.set_pipeline(&self.compose_pipeline);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                continue;
            }
            let texture = self.layers[layer].as_ref().expect("checked layer texture");
            let bind_group = match self.sampling_quality {
                ViewerSamplingQuality::Nearest | ViewerSamplingQuality::Bicubic => {
                    &texture.nearest_bind_group
                }
                ViewerSamplingQuality::Bilinear => &texture.bilinear_bind_group,
            };
            pass.set_bind_group(0, bind_group, &[]);
            let start = (layer * VERTICES_PER_LAYER) as u32;
            pass.draw(start..start + count, 0..1);
            pass.set_pipeline(&self.matte_pipeline);
            pass.set_vertex_buffer(0, self.matte_vertex_buffer.slice(..));
            pass.draw(
                0..VERTICES_PER_LAYER as u32,
                (layer + 1) as u32..(layer + 2) as u32,
            );
            pass.set_pipeline(&self.compose_pipeline);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        }
        drop(pass);
        if record_gpu_timing {
            let timestamps = self
                .gpu_timestamps
                .as_ref()
                .expect("timing resources reserved");
            encoder.resolve_query_set(
                &timestamps.query_set,
                0..TIMESTAMP_QUERY_COUNT,
                &timestamps.resolve_buffer,
                0,
            );
            encoder.copy_buffer_to_buffer(
                &timestamps.resolve_buffer,
                0,
                &timestamps.readback_buffer,
                0,
                TIMESTAMP_RESULT_BYTES,
            );
            let mailbox = Arc::clone(&timestamps.mailbox);
            encoder.map_buffer_on_submit(
                &timestamps.readback_buffer,
                wgpu::MapMode::Read,
                0..TIMESTAMP_RESULT_BYTES,
                move |result| mailbox.complete(result.is_ok()),
            );
        }
        self.front_output = back;
        self.applied_frame_generation = frame_generation;
        self.applied_input_generation = self.input_generation;
        true
    }

    fn paint(&self, pass: &mut wgpu::RenderPass<'_>, info: egui::PaintCallbackInfo) {
        let Some(outputs) = &self.outputs else {
            return;
        };
        let viewport = info.viewport_in_pixels();
        let clip = info.clip_rect_in_pixels();
        if viewport.width_px <= 0
            || viewport.height_px <= 0
            || clip.width_px <= 0
            || clip.height_px <= 0
        {
            return;
        }
        pass.set_pipeline(&self.blit_pipeline);
        pass.set_bind_group(0, &outputs[self.front_output].blit_bind_group, &[]);
        pass.set_viewport(
            viewport.left_px as f32,
            viewport.top_px as f32,
            viewport.width_px as f32,
            viewport.height_px as f32,
            0.0,
            1.0,
        );
        pass.set_scissor_rect(
            clip.left_px as u32,
            clip.top_px as u32,
            clip.width_px as u32,
            clip.height_px as u32,
        );
        pass.draw(0..6, 0..1);
        self.presentation.record_paint();
    }

    fn ensure_outputs(&mut self, device: &wgpu::Device, size: PixelSize) -> bool {
        if self.output_size == Some(size) && self.outputs.is_some() {
            return false;
        }
        if let (Some(outputs), Some(previous_size)) = (self.outputs.take(), self.output_size) {
            self.pool_outputs(previous_size, outputs);
        }
        self.outputs = Some(self.take_outputs(size).unwrap_or_else(|| {
            self.pool_counters.output_allocations += 1;
            std::array::from_fn(|_| {
                create_output(device, &self.texture_layout, &self.bilinear_sampler, size)
            })
        }));
        self.output_size = Some(size);
        true
    }

    fn take_layer(&mut self, size: PixelSize) -> Option<LayerTexture> {
        let index = self
            .layer_pool
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.size == size))?;
        let entry = self.layer_pool[index].take().expect("pooled layer present");
        self.pooled_bytes = self.pooled_bytes.saturating_sub(entry.bytes);
        self.pool_counters.layer_reuses += 1;
        Some(entry.texture)
    }

    fn take_outputs(&mut self, size: PixelSize) -> Option<[OutputTarget; 2]> {
        if self.output_pool.as_ref()?.size != size {
            return None;
        }
        let entry = self.output_pool.take().expect("pooled outputs present");
        self.pooled_bytes = self.pooled_bytes.saturating_sub(entry.bytes);
        self.pool_counters.output_reuses += 1;
        Some(entry.outputs)
    }

    fn pool_layer(&mut self, size: PixelSize, texture: LayerTexture) {
        let Some(bytes) = layer_resource_bytes(size) else {
            self.pool_counters.rejected_oversize += 1;
            return;
        };
        if bytes > RESOURCE_POOL_BYTE_CAP {
            self.pool_counters.rejected_oversize += 1;
            return;
        }
        while self.layer_pool.iter().flatten().count() >= MAX_POOLED_LAYER_BUNDLES
            || self.pooled_bytes.saturating_add(bytes) > RESOURCE_POOL_BYTE_CAP
        {
            if !self.evict_oldest_pooled_resource() {
                self.pool_counters.rejected_oversize += 1;
                return;
            }
        }
        let slot = self
            .layer_pool
            .iter()
            .position(Option::is_none)
            .expect("pool capacity checked");
        self.layer_pool[slot] = Some(PooledLayerTexture {
            size,
            bytes,
            order: self.next_pool_order(),
            texture,
        });
        self.pooled_bytes += bytes;
    }

    fn pool_outputs(&mut self, size: PixelSize, outputs: [OutputTarget; 2]) {
        let Some(bytes) = output_pair_resource_bytes(size) else {
            self.pool_counters.rejected_oversize += 1;
            return;
        };
        if bytes > RESOURCE_POOL_BYTE_CAP {
            self.pool_counters.rejected_oversize += 1;
            return;
        }
        while usize::from(self.output_pool.is_some()) >= MAX_POOLED_OUTPUT_PAIRS
            || self.pooled_bytes.saturating_add(bytes) > RESOURCE_POOL_BYTE_CAP
        {
            if !self.evict_oldest_pooled_resource() {
                self.pool_counters.rejected_oversize += 1;
                return;
            }
        }
        self.output_pool = Some(PooledOutputTargets {
            size,
            bytes,
            order: self.next_pool_order(),
            outputs,
        });
        self.pooled_bytes += bytes;
    }

    fn evict_oldest_pooled_resource(&mut self) -> bool {
        let oldest_layer = self
            .layer_pool
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry.order)))
            .min_by_key(|(_, order)| *order);
        let output_order = self.output_pool.as_ref().map(|entry| entry.order);
        if let Some((index, layer_order)) = oldest_layer
            && output_order.is_none_or(|order| layer_order <= order)
        {
            let entry = self.layer_pool[index].take().expect("pooled layer present");
            self.pooled_bytes = self.pooled_bytes.saturating_sub(entry.bytes);
            self.pool_counters.evictions += 1;
            return true;
        }
        if let Some(entry) = self.output_pool.take() {
            self.pooled_bytes = self.pooled_bytes.saturating_sub(entry.bytes);
            self.pool_counters.evictions += 1;
            return true;
        }
        false
    }

    fn next_pool_order(&mut self) -> u64 {
        let order = self.next_pool_order;
        self.next_pool_order = self.next_pool_order.wrapping_add(1);
        order
    }

    #[cfg(test)]
    fn pool_counters(&self) -> ViewerCompositorPoolCounters {
        self.pool_counters
    }
}

fn texture_resource_bytes(size: PixelSize, bytes_per_pixel: u64) -> Option<u64> {
    u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
}

fn layer_resource_bytes(size: PixelSize) -> Option<u64> {
    texture_resource_bytes(size, 8)?.checked_add(
        (std::mem::size_of::<GpuColorCorrectionStack>() + std::mem::size_of::<GpuCurveLutStack>())
            as u64,
    )
}

fn output_pair_resource_bytes(size: PixelSize) -> Option<u64> {
    texture_resource_bytes(size, 8)?.checked_add(
        (2 * (std::mem::size_of::<GpuColorCorrectionStack>()
            + std::mem::size_of::<GpuCurveLutStack>())) as u64,
    )
}

fn validate_layer(layer: usize) -> Result<(), ViewerUploadError> {
    if layer < MAX_COMPOSITE_LAYERS {
        Ok(())
    } else {
        Err(ViewerUploadError::LayerOutOfBounds { layer })
    }
}
fn validate_upload(
    layer: usize,
    width: u32,
    height: u32,
    actual: usize,
) -> Result<(), ViewerUploadError> {
    validate_layer(layer)?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ViewerUploadError::ZeroDimension)?;
    if width == 0 || height == 0 {
        return Err(ViewerUploadError::ZeroDimension);
    }
    if actual != expected {
        return Err(ViewerUploadError::InvalidRgbaLength { expected, actual });
    }
    Ok(())
}

fn texture_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("viewer compositor texture layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<GpuCurveLutStack>() as u64,
                    ),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<
                        GpuColorCorrectionStack,
                    >() as u64),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn premultiply_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("viewer premultiply texture layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn premultiplied_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

fn premultiply_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    texture_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("viewer premultiply pipeline layout"),
        bind_group_layouts: &[Some(texture_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("viewer premultiply pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_blit"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_premultiply"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn compose_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    texture_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(texture_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[viewer_vertex_layout()],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(premultiplied_blend()),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn matte_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_matte"),
            compilation_options: Default::default(),
            buffers: &[matte_vertex_layout()],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_matte"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(premultiplied_blend()),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn blit_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    texture_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(texture_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_blit"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(blit_fragment_entry_point(format)),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn blit_fragment_entry_point(format: wgpu::TextureFormat) -> &'static str {
    if format.is_srgb() {
        "fs_blit_srgb"
    } else {
        "fs_blit_encoded"
    }
}

const fn composition_required(
    resized: bool,
    applied_frame_generation: u64,
    frame_generation: u64,
    applied_input_generation: u64,
    input_generation: u64,
) -> bool {
    resized
        || applied_frame_generation != frame_generation
        || applied_input_generation != input_generation
}

fn physical_canvas_size(rect: egui::Rect, pixels_per_point: f32, screen: [u32; 2]) -> PixelSize {
    let scale = pixels_per_point.max(f32::EPSILON);
    let screen_width = screen[0].max(1) as f32;
    let screen_height = screen[1].max(1) as f32;
    let left = (rect.min.x * scale).floor().clamp(0.0, screen_width);
    let top = (rect.min.y * scale).floor().clamp(0.0, screen_height);
    let right = (rect.max.x * scale).ceil().clamp(0.0, screen_width);
    let bottom = (rect.max.y * scale).ceil().clamp(0.0, screen_height);
    PixelSize::new(
        (right - left).max(1.0) as u32,
        (bottom - top).max(1.0) as u32,
    )
}

fn viewer_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x2,
        2 => Float32
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ViewerVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &ATTRIBUTES,
    }
}

fn matte_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRIBUTES: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32, 1 => Float32x3];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<MatteVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

fn create_layer_texture(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    premultiply_layout: &wgpu::BindGroupLayout,
    nearest_sampler: &wgpu::Sampler,
    bilinear_sampler: &wgpu::Sampler,
    width: u32,
    height: u32,
) -> LayerTexture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer compositor input"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let premultiplied_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer compositor premultiplied input"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let premultiplied_view = premultiplied_texture.create_view(&Default::default());
    let correction_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer color correction uniform"),
        size: std::mem::size_of::<GpuColorCorrectionStack>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let curve_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer color curve storage"),
        size: std::mem::size_of::<GpuCurveLutStack>() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let nearest_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer compositor nearest input bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&premultiplied_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(nearest_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: correction_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: curve_buffer.as_entire_binding(),
            },
        ],
    });
    let bilinear_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer compositor bilinear input bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&premultiplied_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(bilinear_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: correction_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: curve_buffer.as_entire_binding(),
            },
        ],
    });
    let premultiply_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer premultiply input bind group"),
        layout: premultiply_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(nearest_sampler),
            },
        ],
    });
    LayerTexture {
        _texture: texture,
        nearest_bind_group,
        bilinear_bind_group,
        premultiply_bind_group,
        premultiplied_view,
        _premultiplied_texture: premultiplied_texture,
        correction_buffer,
        curve_buffer,
        premultiply_needed: true,
    }
}

fn create_output(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    size: PixelSize,
) -> OutputTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer compositor output"),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let correction_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer blit neutral correction uniform"),
        size: std::mem::size_of::<GpuColorCorrectionStack>() as u64,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: false,
    });
    let curve_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer blit neutral curve storage"),
        size: std::mem::size_of::<GpuCurveLutStack>() as u64,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer compositor output bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: correction_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: curve_buffer.as_entire_binding(),
            },
        ],
    });
    OutputTarget {
        _texture: texture,
        view,
        blit_bind_group,
        _correction_buffer: correction_buffer,
        _curve_buffer: curve_buffer,
    }
}

fn write_vertices(
    destination: &mut [ViewerVertex],
    primitive: ViewerLayerPrimitive,
    project: PixelSize,
) {
    let indices = [0, 1, 2, 2, 3, 0];
    for (out, index) in destination.iter_mut().zip(indices) {
        let position = primitive.quad.positions[index];
        let quad_uv = primitive.quad.uvs[index];
        *out = ViewerVertex {
            position: [
                position.x / project.width as f32 * 2.0 - 1.0,
                1.0 - position.y / project.height as f32 * 2.0,
            ],
            uv: bilerp_uv(primitive.content_uv, quad_uv),
            opacity: primitive.quad.opacity,
        };
    }
}

fn gpu_color_correction_stack(primitive: ViewerLayerPrimitive) -> GpuColorCorrectionStack {
    let count = primitive
        .color_correction_count
        .min(MAX_COLOR_CORRECTIONS_PER_LAYER as u32);
    let mut corrections = [GpuColorCorrection {
        color: [0.0, 0.0, 1.0, 0.0],
        light: [0.0, 1.0, 0.0, 0.0],
        effect: [0.0, 0.0, 0.5, 0.0],
        center: [0.0; 4],
    }; MAX_COLOR_CORRECTIONS_PER_LAYER];
    for (destination, source) in corrections
        .iter_mut()
        .zip(primitive.color_corrections.iter())
        .take(count as usize)
    {
        *destination = GpuColorCorrection {
            color: [
                sanitize_unit_offset(source.temperature),
                sanitize_unit_offset(source.tint),
                sanitize_saturation(source.saturation),
                sanitize_exposure(source.exposure),
            ],
            light: [
                sanitize_brightness(source.brightness),
                sanitize_contrast(source.contrast),
                sanitize_unit_offset(source.highlights),
                sanitize_unit_offset(source.shadows),
            ],
            effect: [
                match source.operation {
                    ViewerColorCorrectionOperation::BasicCorrection => 0.0,
                    ViewerColorCorrectionOperation::Vignette => 1.0,
                },
                sanitize_vignette_amount(source.vignette_amount),
                sanitize_vignette_radius(source.vignette_midpoint),
                sanitize_vignette_radius(source.vignette_feather),
            ],
            center: [
                sanitize_unit_offset(source.vignette_center_x),
                sanitize_unit_offset(source.vignette_center_y),
                sanitize_unit_offset(source.whites),
                sanitize_unit_offset(source.blacks),
            ],
        };
    }
    GpuColorCorrectionStack {
        corrections,
        count,
        sampling_quality: ViewerSamplingQuality::Nearest.shader_value(),
        _padding: [0; 2],
    }
}

fn gpu_curve_lut_stack(primitive: ViewerLayerPrimitive) -> GpuCurveLutStack {
    let mut samples = std::array::from_fn(|index| {
        let level = (index % COLOR_CURVE_LUT_SAMPLES) as f32 / (COLOR_CURVE_LUT_SAMPLES - 1) as f32;
        [level, level, level, 0.0]
    });
    let count = primitive
        .color_correction_count
        .min(MAX_COLOR_CORRECTIONS_PER_LAYER as u32) as usize;
    for (node, correction) in primitive.color_corrections.iter().take(count).enumerate() {
        let start = node * COLOR_CURVE_LUT_SAMPLES;
        let master = NaturalCurveSpline::from_curve(correction.curves.master);
        let red = NaturalCurveSpline::from_curve(correction.curves.red);
        let green = NaturalCurveSpline::from_curve(correction.curves.green);
        let blue = NaturalCurveSpline::from_curve(correction.curves.blue);
        for (index, destination) in samples[start..start + COLOR_CURVE_LUT_SAMPLES]
            .iter_mut()
            .enumerate()
        {
            let input = index as f32 / (COLOR_CURVE_LUT_SAMPLES - 1) as f32;
            *destination = [
                master.sample(red.sample(input)),
                master.sample(green.sample(input)),
                master.sample(blue.sample(input)),
                0.0,
            ];
        }
    }
    GpuCurveLutStack { samples }
}

#[derive(Clone, Copy)]
struct NaturalCurveSpline {
    points: [[f32; 2]; MAX_COLOR_CURVE_POINTS],
    b: [f32; MAX_COLOR_CURVE_POINTS - 1],
    c: [f32; MAX_COLOR_CURVE_POINTS],
    d: [f32; MAX_COLOR_CURVE_POINTS - 1],
    count: usize,
}

impl NaturalCurveSpline {
    fn from_curve(curve: ViewerColorCurve) -> Self {
        Self::try_from_curve(curve).unwrap_or_else(Self::identity)
    }

    fn try_from_curve(curve: ViewerColorCurve) -> Option<Self> {
        let count = curve.count as usize;
        if !(2..=MAX_COLOR_CURVE_POINTS).contains(&count) {
            return None;
        }
        let points = curve.points;
        if points[0][0] != 0.0 || points[count - 1][0] != 1.0 {
            return None;
        }
        if points[..count].iter().any(|point| {
            !point[0].is_finite()
                || !point[1].is_finite()
                || !(0.0..=1.0).contains(&point[0])
                || !(0.0..=1.0).contains(&point[1])
        }) || points[..count]
            .windows(2)
            .any(|pair| ((pair[0][0] * 255.0) as i32) >= ((pair[1][0] * 255.0) as i32))
        {
            return None;
        }

        let mut h = [0.0; MAX_COLOR_CURVE_POINTS - 1];
        let mut alpha = [0.0; MAX_COLOR_CURVE_POINTS];
        for index in 0..count - 1 {
            h[index] = points[index + 1][0] - points[index][0];
        }
        for index in 1..count - 1 {
            alpha[index] = 3.0 / h[index] * (points[index + 1][1] - points[index][1])
                - 3.0 / h[index - 1] * (points[index][1] - points[index - 1][1]);
        }
        let mut lower = [0.0; MAX_COLOR_CURVE_POINTS];
        let mut mu = [0.0; MAX_COLOR_CURVE_POINTS];
        let mut z = [0.0; MAX_COLOR_CURVE_POINTS];
        lower[0] = 1.0;
        for index in 1..count - 1 {
            lower[index] =
                2.0 * (points[index + 1][0] - points[index - 1][0]) - h[index - 1] * mu[index - 1];
            mu[index] = h[index] / lower[index];
            z[index] = (alpha[index] - h[index - 1] * z[index - 1]) / lower[index];
        }
        let mut c = [0.0; MAX_COLOR_CURVE_POINTS];
        let mut b = [0.0; MAX_COLOR_CURVE_POINTS - 1];
        let mut d = [0.0; MAX_COLOR_CURVE_POINTS - 1];
        for index in (0..count - 1).rev() {
            c[index] = z[index] - mu[index] * c[index + 1];
            b[index] = (points[index + 1][1] - points[index][1]) / h[index]
                - h[index] * (c[index + 1] + 2.0 * c[index]) / 3.0;
            d[index] = (c[index + 1] - c[index]) / (3.0 * h[index]);
        }
        if b[..count - 1].iter().any(|value| !value.is_finite())
            || c[..count].iter().any(|value| !value.is_finite())
            || d[..count - 1].iter().any(|value| !value.is_finite())
        {
            return None;
        }
        Some(Self {
            points,
            b,
            c,
            d,
            count,
        })
    }

    fn identity() -> Self {
        Self::try_from_curve(ViewerColorCurve::default()).expect("identity curve is valid")
    }

    fn sample(&self, input: f32) -> f32 {
        let input = input.clamp(0.0, 1.0);
        let index = self.points[..self.count]
            .partition_point(|point| point[0] <= input)
            .saturating_sub(1)
            .min(self.count - 2);
        let distance = input - self.points[index][0];
        (self.points[index][1]
            + self.b[index] * distance
            + self.c[index] * distance * distance
            + self.d[index] * distance.powi(3))
        .clamp(0.0, 1.0)
    }
}

fn sanitize_unit_offset(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn sanitize_saturation(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 2.0)
    } else {
        1.0
    }
}

fn sanitize_exposure(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-5.0, 5.0)
    } else {
        0.0
    }
}

fn sanitize_brightness(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn sanitize_contrast(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 4.0)
    } else {
        1.0
    }
}

fn sanitize_vignette_amount(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn sanitize_vignette_radius(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
fn vignette_multiplier(
    uv: [f32; 2],
    amount: f32,
    midpoint: f32,
    feather: f32,
    center_x: f32,
    center_y: f32,
) -> f32 {
    let dx = 2.0 * (uv[0] - (0.5 + center_x * 0.5));
    let dy = 2.0 * (uv[1] - (0.5 + center_y * 0.5));
    let radius = (dx * dx + dy * dy).sqrt() / 2.0_f32.sqrt();
    let outer = midpoint + feather * (1.0 - midpoint);
    let t = ((radius - midpoint) / (outer - midpoint).max(0.0001)).clamp(0.0, 1.0);
    let smooth = t * t * (3.0 - 2.0 * t);
    1.0 - amount * smooth
}

fn sanitize_matte_opacity(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn matte_vertices(
    black_opacities: [f32; MAX_COMPOSITE_LAYERS + 1],
    white_opacities: [f32; MAX_COMPOSITE_LAYERS + 1],
) -> [MatteVertex; MAX_COMPOSITE_LAYERS + 1] {
    std::array::from_fn(|index| {
        let black = sanitize_matte_opacity(black_opacities[index]);
        let white = sanitize_matte_opacity(white_opacities[index]);
        let opacity = 1.0 - (1.0 - black) * (1.0 - white);
        let luminance = if opacity > 0.0 { white / opacity } else { 0.0 };
        MatteVertex {
            opacity,
            color: [luminance; 3],
        }
    })
}

fn bilerp_uv(corners: [Uv; 4], point: Uv) -> [f32; 2] {
    let top = lerp_uv(corners[0], corners[1], point.u);
    let bottom = lerp_uv(corners[3], corners[2], point.u);
    let uv = lerp_uv(top, bottom, point.v);
    [uv.u, uv.v]
}
fn lerp_uv(a: Uv, b: Uv, t: f32) -> Uv {
    Uv {
        u: a.u + (b.u - a.u) * t,
        v: a.v + (b.v - a.v) * t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_compositor::{Point, Uv};
    use nle_timeline::ClipId;
    use serde::Serialize;
    use std::{
        fs,
        path::Path,
        sync::mpsc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    const CROSS_ADAPTER_REPORT_ENV: &str = "MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT";
    const QUALIFICATION_SOURCE_SIZE: PixelSize = PixelSize::new(1_920, 1_080);
    const QUALIFICATION_OUTPUT_SIZE: PixelSize = PixelSize::new(1_920, 1_080);
    const QUALIFICATION_WARMUP_SUBMISSIONS: u32 = 5;
    const QUALIFICATION_MEASURED_SUBMISSIONS: u32 = 30;
    const QUALIFICATION_FRAME_BUDGET_MS: f32 = 1_000.0 / 30.0;
    const QUALIFICATION_READBACK_TOLERANCE: u8 = 4;

    #[derive(Debug, Serialize)]
    struct CrossAdapterReport {
        schema_version: u32,
        status: &'static str,
        scope: &'static str,
        physical_scanout_observed: bool,
        app_auto_preview_observed: bool,
        machine: CrossAdapterMachine,
        workload: CrossAdapterWorkload,
        adapters: Vec<CrossAdapterEvidence>,
    }

    #[derive(Debug, Serialize)]
    struct CrossAdapterMachine {
        computer_name: Option<String>,
        os: String,
        process_architecture: String,
        processor_count: usize,
    }

    #[derive(Debug, Serialize)]
    struct CrossAdapterEvidence {
        name: String,
        vendor: u32,
        device: u32,
        device_type: String,
        backend: String,
        driver: String,
        driver_info: String,
        timestamp_query_supported: bool,
        layer_count: u32,
        correctness_readback_passed: bool,
        correctness_actual_rgba: [u8; 4],
        correctness_expected_rgba: [u8; 4],
        correctness_tolerance: u8,
        warmup_submissions: u32,
        measured_submissions: u32,
        cpu_encode_timing: CrossAdapterTiming,
        gpu_pass_timing: CrossAdapterTiming,
        state_scenarios: Vec<CrossAdapterStateScenario>,
    }

    /// Evidence from a production-renderer state transition performed after the
    /// timed workload. These transitions deliberately do not contribute to the
    /// timing window; they prove the retained compositor reacts to declaration
    /// changes and source availability without a hidden re-upload.
    #[derive(Debug, Serialize)]
    struct CrossAdapterStateScenario {
        name: &'static str,
        generation: u64,
        actual_rgba: [u8; 4],
        expected_rgba: [u8; 4],
        correctness_passed: bool,
        probes: Vec<CrossAdapterReadbackProbe>,
        uploads_performed: bool,
        upload_serials_before: [u64; MAX_COMPOSITE_LAYERS],
        upload_serials_after: [u64; MAX_COMPOSITE_LAYERS],
        composed_upload_serials: [Option<u64>; MAX_COMPOSITE_LAYERS],
        composition_matches_current_uploads: bool,
        top_layer_composed: bool,
    }

    #[derive(Debug, Serialize)]
    struct CrossAdapterReadbackProbe {
        x: u32,
        y: u32,
        actual_rgba: [u8; 4],
        expected_rgba: [u8; 4],
        correctness_passed: bool,
    }

    #[derive(Debug, PartialEq, Serialize)]
    struct CrossAdapterTiming {
        samples: usize,
        p95_ms: f32,
        max_ms: f32,
    }

    #[derive(Debug, Serialize)]
    struct CrossAdapterWorkload {
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
        sampling: &'static str,
        warmup_submissions: u32,
        measured_submissions: u32,
        target_fps: u32,
        frame_budget_ms: f32,
        uploads_excluded_from_timing: bool,
        warmup_excluded_from_timing: bool,
    }

    fn cross_adapter_machine() -> CrossAdapterMachine {
        CrossAdapterMachine {
            computer_name: std::env::var("COMPUTERNAME").ok(),
            os: std::env::consts::OS.to_owned(),
            process_architecture: std::env::consts::ARCH.to_owned(),
            processor_count: std::thread::available_parallelism()
                .map(|parallelism| parallelism.get())
                .unwrap_or(1),
        }
    }

    fn write_cross_adapter_report(path: &Path, report: &CrossAdapterReport) -> Result<(), String> {
        if !path.is_absolute() {
            return Err(format!(
                "{CROSS_ADAPTER_REPORT_ENV} must be an absolute path"
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| "cross-adapter report path has no parent directory".to_owned())?;
        fs::create_dir_all(parent).map_err(|error| format!("create report directory: {error}"))?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("clock for report nonce: {error}"))?
            .as_nanos();
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("phase0-cross-adapter.json"),
            std::process::id(),
            nonce
        ));
        let serialized = serde_json::to_vec_pretty(report)
            .map_err(|error| format!("serialize cross-adapter report: {error}"))?;
        fs::write(&temporary, serialized)
            .map_err(|error| format!("write temporary report: {error}"))?;
        fs::rename(&temporary, path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!("atomically publish cross-adapter report: {error}")
        })
    }

    fn adapter_label(info: &wgpu::AdapterInfo) -> String {
        format!(
            "{} ({:?}, {:?}, vendor=0x{:04X}, device=0x{:04X})",
            info.name, info.device_type, info.backend, info.vendor, info.device
        )
    }

    fn qualification_frame(layer_count: usize) -> ViewerFrame {
        let transformed_quad = |id| CompositeQuad {
            clip_id: ClipId(id),
            positions: [
                Point { x: -96.0, y: -54.0 },
                Point {
                    x: 2_016.0,
                    y: -81.0,
                },
                Point {
                    x: 2_054.0,
                    y: 1_134.0,
                },
                Point {
                    x: -58.0,
                    y: 1_161.0,
                },
            ],
            uvs: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            opacity: 1.0,
        };
        let primitive = |id, opacity| ViewerLayerPrimitive {
            quad: CompositeQuad {
                opacity,
                ..transformed_quad(id)
            },
            content_uv: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            color_corrections: [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER],
            color_correction_count: 0,
        };
        let mut layers = [None; MAX_COMPOSITE_LAYERS];
        for (index, opacity) in [0.55, 0.50, 0.30, 0.20]
            .into_iter()
            .enumerate()
            .take(layer_count)
        {
            layers[index] = Some(primitive(index as u32 + 1, opacity));
        }
        ViewerFrame {
            project_size: QUALIFICATION_OUTPUT_SIZE,
            logical_canvas_rect: egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1_920.0, 1_080.0),
            ),
            black_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers,
        }
    }

    fn qualification_frame_top_off_center(layer_count: usize) -> ViewerFrame {
        let mut frame = qualification_frame(layer_count);
        let top = frame.layers[layer_count - 1]
            .as_mut()
            .expect("top qualification layer");
        top.quad.positions = [
            Point {
                x: 1_700.0,
                y: 20.0,
            },
            Point {
                x: 1_900.0,
                y: 20.0,
            },
            Point {
                x: 1_900.0,
                y: 220.0,
            },
            Point {
                x: 1_700.0,
                y: 220.0,
            },
        ];
        frame
    }

    fn qualification_frame_without_top(layer_count: usize) -> ViewerFrame {
        let mut frame = qualification_frame(layer_count);
        frame.layers[layer_count - 1] = None;
        frame
    }

    fn black_matte_qualification_frame() -> ViewerFrame {
        let full_quad = |id| CompositeQuad {
            clip_id: ClipId(id),
            positions: [
                Point { x: 0.0, y: 0.0 },
                Point { x: 4.0, y: 0.0 },
                Point { x: 4.0, y: 4.0 },
                Point { x: 0.0, y: 4.0 },
            ],
            uvs: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            opacity: 1.0,
        };
        let primitive = |id, opacity| ViewerLayerPrimitive {
            quad: CompositeQuad {
                opacity,
                ..full_quad(id)
            },
            content_uv: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            color_corrections: [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER],
            color_correction_count: 0,
        };
        let mut corrected_base = primitive(1, 1.0);
        corrected_base.color_corrections[0] = ViewerColorCorrection {
            brightness: 0.8,
            contrast: 1.0,
            ..Default::default()
        };
        corrected_base.color_corrections[1] = ViewerColorCorrection {
            brightness: -0.5,
            contrast: 1.0,
            ..Default::default()
        };
        corrected_base.color_correction_count = 2;
        ViewerFrame {
            project_size: PixelSize::new(4, 4),
            logical_canvas_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(4.0, 4.0)),
            black_mattes_before: [0.0, 1.0, 0.0, 0.0, 0.0],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers: [Some(corrected_base), Some(primitive(2, 0.5)), None, None],
        }
    }

    fn readback_first_pixel(
        device: &wgpu::Device,
        buffer: &wgpu::Buffer,
        submission: wgpu::SubmissionIndex,
    ) -> Result<[u8; 4], String> {
        let (sent, received) = mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sent.send(result);
            });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(2)),
            })
            .map_err(|error| format!("viewer GPU completion: {error}"))?;
        received
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| format!("readback callback: {error}"))?
            .map_err(|error| format!("mapped output: {error}"))?;
        let pixels = buffer.slice(..).get_mapped_range();
        let pixel = pixels
            .get(..4)
            .ok_or_else(|| "viewer GPU readback omitted its first pixel".to_owned())?
            .try_into()
            .expect("four-byte pixel");
        drop(pixels);
        buffer.unmap();
        Ok(pixel)
    }

    fn readback_center_pixel(
        device: &wgpu::Device,
        buffer: &wgpu::Buffer,
        submission: wgpu::SubmissionIndex,
    ) -> Result<[u8; 4], String> {
        readback_first_pixel(device, buffer, submission)
    }

    fn readback_probe_pixels(
        device: &wgpu::Device,
        buffer: &wgpu::Buffer,
        submission: wgpu::SubmissionIndex,
        count: usize,
    ) -> Result<Vec<[u8; 4]>, String> {
        let (sent, received) = mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sent.send(result);
            });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(2)),
            })
            .map_err(|error| format!("viewer GPU completion: {error}"))?;
        received
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| format!("readback callback: {error}"))?
            .map_err(|error| format!("mapped output: {error}"))?;
        let bytes = buffer.slice(..).get_mapped_range();
        let pixels = (0..count)
            .map(|index| {
                let offset = index * 256;
                bytes
                    .get(offset..offset + 4)
                    .ok_or_else(|| "viewer GPU readback omitted a probe pixel".to_owned())?
                    .try_into()
                    .map_err(|_| "viewer GPU probe pixel was not four bytes".to_owned())
            })
            .collect::<Result<Vec<[u8; 4]>, String>>();
        drop(bytes);
        buffer.unmap();
        pixels
    }

    fn verify_black_matte_readback(
        device: &wgpu::Device,
        buffer: &wgpu::Buffer,
        submission: wgpu::SubmissionIndex,
    ) -> Result<(), String> {
        let pixel = readback_first_pixel(device, buffer, submission)?;
        if pixel[..3]
            .iter()
            .all(|channel| (125..=130).contains(channel))
        {
            Ok(())
        } else {
            Err(
                "opaque boundary-one matte did not cover lower media before 50% white upper layer"
                    .to_owned(),
            )
        }
    }

    fn qualification_expected_rgba(layer_count: usize) -> [u8; 4] {
        let colors: [[f32; 3]; 4] = [
            [255.0, 0.0, 0.0],
            [0.0, 255.0, 0.0],
            [0.0, 0.0, 255.0],
            [255.0; 3],
        ];
        let opacities = [0.55, 0.50, 0.30, 0.20];
        let mut result = [0.0_f32; 3];
        for (color, opacity) in colors.into_iter().zip(opacities).take(layer_count) {
            for channel in 0..3 {
                result[channel] = color[channel] * opacity + result[channel] * (1.0 - opacity);
            }
        }
        [
            result[0].round() as u8,
            result[1].round() as u8,
            result[2].round() as u8,
            255,
        ]
    }

    fn qualification_layer_rgba(layer: usize) -> [u8; 4] {
        [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ][layer]
    }

    fn rgba_within_tolerance(actual: [u8; 4], expected: [u8; 4], tolerance: u8) -> bool {
        actual
            .into_iter()
            .zip(expected)
            .all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
    }

    fn timing_from_samples(samples: &mut [f32]) -> Result<CrossAdapterTiming, String> {
        if samples.is_empty()
            || samples
                .iter()
                .any(|sample| !sample.is_finite() || *sample < 0.0)
        {
            return Err("qualification timing samples must be finite and non-negative".to_owned());
        }
        samples.sort_unstable_by(f32::total_cmp);
        let p95_index = samples
            .len()
            .saturating_mul(95)
            .div_ceil(100)
            .saturating_sub(1);
        Ok(CrossAdapterTiming {
            samples: samples.len(),
            p95_ms: samples[p95_index],
            max_ms: samples[samples.len() - 1],
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_cross_adapter_state_scenario(
        renderer: &mut ViewerCompositorRenderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        readback: &wgpu::Buffer,
        adapter_info: &wgpu::AdapterInfo,
        top_layer: usize,
        state_scenarios: &mut Vec<CrossAdapterStateScenario>,
        name: &'static str,
        frame: ViewerFrame,
        generation: u64,
        probes: &[(u32, u32, [u8; 4])],
        uploads_performed: bool,
        upload_serials_before: [u64; MAX_COMPOSITE_LAYERS],
    ) -> Result<(), String> {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        if !renderer.prepare(
            device,
            queue,
            &mut encoder,
            Some(frame),
            generation,
            QUALIFICATION_OUTPUT_SIZE,
        ) {
            return Err(format!(
                "state scenario {name} generation {generation} was skipped"
            ));
        }
        let output = &renderer
            .outputs
            .as_ref()
            .expect("output after state prepare")[renderer.front_output]
            ._texture;
        for (index, &(x, y, _)) in probes.iter().enumerate() {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: output,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: (index * 256) as u64,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(1),
                    },
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
        }
        let actual_pixels = readback_probe_pixels(
            device,
            readback,
            queue.submit([encoder.finish()]),
            probes.len(),
        )?;
        let probes = probes
            .iter()
            .copied()
            .zip(actual_pixels)
            .map(
                |((x, y, expected_rgba), actual_rgba)| CrossAdapterReadbackProbe {
                    x,
                    y,
                    actual_rgba,
                    expected_rgba,
                    correctness_passed: rgba_within_tolerance(
                        actual_rgba,
                        expected_rgba,
                        QUALIFICATION_READBACK_TOLERANCE,
                    ),
                },
            )
            .collect::<Vec<_>>();
        let actual_rgba = probes[0].actual_rgba;
        let expected_rgba = probes[0].expected_rgba;
        let evidence = renderer.presentation_evidence();
        let correctness_passed = probes.iter().all(|probe| probe.correctness_passed);
        if !correctness_passed {
            let failed = probes
                .iter()
                .find(|probe| !probe.correctness_passed)
                .expect("failed scenario has a failed probe");
            return Err(format!(
                "{} state scenario {name} probe ({}, {}) readback {:?} did not match {:?} within {}",
                adapter_label(adapter_info),
                failed.x,
                failed.y,
                failed.actual_rgba,
                failed.expected_rgba,
                QUALIFICATION_READBACK_TOLERANCE
            ));
        }
        let composed_upload_serials = renderer.presentation.composed_upload_serials;
        let top_layer_composed = composed_upload_serials[top_layer].is_some();
        state_scenarios.push(CrossAdapterStateScenario {
            name,
            generation,
            actual_rgba,
            expected_rgba,
            correctness_passed,
            probes,
            uploads_performed,
            upload_serials_before,
            upload_serials_after: evidence.upload_serials,
            composed_upload_serials,
            composition_matches_current_uploads: composed_upload_serials
                .iter()
                .zip(evidence.upload_serials)
                .all(|(painted, uploaded)| painted.is_none_or(|serial| serial == uploaded)),
            top_layer_composed,
        });
        Ok(())
    }

    fn qualify_viewer_compositor_adapter(
        adapter: wgpu::Adapter,
        layer_count: usize,
    ) -> Result<CrossAdapterEvidence, String> {
        let info = adapter.get_info();
        if !adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return Err(format!(
                "{} does not support required TIMESTAMP_QUERY",
                adapter_label(&info)
            ));
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("phase0 cross-adapter viewer compositor device"),
            required_features: wgpu::Features::TIMESTAMP_QUERY,
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|error| format!("request device for {}: {error}", adapter_label(&info)))?;
        let mut renderer = ViewerCompositorRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        let source_pixels =
            (QUALIFICATION_SOURCE_SIZE.width * QUALIFICATION_SOURCE_SIZE.height) as usize;
        for layer in 0..layer_count {
            renderer
                .upload_layer_rgba(
                    &device,
                    &queue,
                    layer,
                    QUALIFICATION_SOURCE_SIZE.width,
                    QUALIFICATION_SOURCE_SIZE.height,
                    &qualification_layer_rgba(layer).repeat(source_pixels),
                )
                .map_err(|error| format!("upload qualification layer {layer}: {error}"))?;
        }
        let frame = qualification_frame(layer_count);
        for generation in 1..=QUALIFICATION_WARMUP_SUBMISSIONS {
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            if !renderer.prepare(
                &device,
                &queue,
                &mut encoder,
                Some(frame),
                generation as u64,
                QUALIFICATION_OUTPUT_SIZE,
            ) {
                return Err(format!(
                    "warmup composition {generation} was unexpectedly skipped"
                ));
            }
            let submission = queue.submit([encoder.finish()]);
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(Duration::from_secs(5)),
                })
                .map_err(|error| format!("warmup GPU completion: {error}"))?;
        }
        renderer.set_gpu_timing_enabled(true);
        let mut cpu_encode_samples =
            Vec::with_capacity(QUALIFICATION_MEASURED_SUBMISSIONS as usize);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("phase0 cross-adapter viewer center readback"),
            size: 512,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut final_submission = None;
        for measured in 0..QUALIFICATION_MEASURED_SUBMISSIONS {
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            let generation = (QUALIFICATION_WARMUP_SUBMISSIONS + measured + 1) as u64;
            let started_at = Instant::now();
            if !renderer.prepare(
                &device,
                &queue,
                &mut encoder,
                Some(frame),
                generation,
                QUALIFICATION_OUTPUT_SIZE,
            ) {
                return Err(format!(
                    "measured composition {generation} was unexpectedly skipped"
                ));
            }
            cpu_encode_samples.push(started_at.elapsed().as_secs_f32() * 1_000.0);
            let output = &renderer.outputs.as_ref().expect("output after prepare")
                [renderer.front_output]
                ._texture;
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: output,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: QUALIFICATION_OUTPUT_SIZE.width / 2,
                        y: QUALIFICATION_OUTPUT_SIZE.height / 2,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(1),
                    },
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            let submission = queue.submit([encoder.finish()]);
            if measured + 1 == QUALIFICATION_MEASURED_SUBMISSIONS {
                final_submission = Some(submission);
            } else {
                device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(submission),
                        timeout: Some(Duration::from_secs(5)),
                    })
                    .map_err(|error| format!("measured GPU completion: {error}"))?;
            }
            renderer.drain_gpu_timing(&queue);
        }
        let actual = readback_center_pixel(
            &device,
            &readback,
            final_submission.expect("final measured submission"),
        )?;
        renderer.drain_gpu_timing(&queue);
        let gpu_timing = renderer.gpu_timing();
        if gpu_timing.samples != QUALIFICATION_MEASURED_SUBMISSIONS as usize {
            return Err(format!(
                "{} produced {} of {} required timestamp samples",
                adapter_label(&info),
                gpu_timing.samples,
                QUALIFICATION_MEASURED_SUBMISSIONS,
            ));
        }
        let expected = qualification_expected_rgba(layer_count);
        if !rgba_within_tolerance(actual, expected, QUALIFICATION_READBACK_TOLERANCE) {
            return Err(format!(
                "{} center readback {actual:?} did not match {expected:?} within {}",
                adapter_label(&info),
                QUALIFICATION_READBACK_TOLERANCE
            ));
        }
        let cpu_encode_timing = timing_from_samples(&mut cpu_encode_samples)?;
        let gpu_pass_timing = CrossAdapterTiming {
            samples: gpu_timing.samples,
            p95_ms: gpu_timing.p95_ms,
            max_ms: gpu_timing.max_ms,
        };
        renderer.set_gpu_timing_enabled(false);
        let initial_upload_serials = renderer.presentation_evidence().upload_serials;
        let top_layer = layer_count - 1;
        let mut state_scenarios = Vec::with_capacity(4);
        let remaining_expected = qualification_expected_rgba(layer_count - 1);
        let first_state_generation =
            (QUALIFICATION_WARMUP_SUBMISSIONS + QUALIFICATION_MEASURED_SUBMISSIONS + 1) as u64;
        run_cross_adapter_state_scenario(
            &mut renderer,
            &device,
            &queue,
            &readback,
            &info,
            top_layer,
            &mut state_scenarios,
            "top_transform_off_center",
            qualification_frame_top_off_center(layer_count),
            first_state_generation,
            &[
                (
                    QUALIFICATION_OUTPUT_SIZE.width / 2,
                    QUALIFICATION_OUTPUT_SIZE.height / 2,
                    remaining_expected,
                ),
                (1_800, 120, expected),
            ],
            false,
            initial_upload_serials,
        )?;
        let before_disable = renderer.presentation_evidence().upload_serials;
        run_cross_adapter_state_scenario(
            &mut renderer,
            &device,
            &queue,
            &readback,
            &info,
            top_layer,
            &mut state_scenarios,
            "top_layer_disabled",
            qualification_frame_without_top(layer_count),
            first_state_generation + 1,
            &[(
                QUALIFICATION_OUTPUT_SIZE.width / 2,
                QUALIFICATION_OUTPUT_SIZE.height / 2,
                remaining_expected,
            )],
            false,
            before_disable,
        )?;
        let before_missing = renderer.presentation_evidence().upload_serials;
        renderer
            .clear_layer(top_layer)
            .map_err(|error| format!("clear top qualification layer: {error}"))?;
        run_cross_adapter_state_scenario(
            &mut renderer,
            &device,
            &queue,
            &readback,
            &info,
            top_layer,
            &mut state_scenarios,
            "top_source_missing",
            qualification_frame(layer_count),
            first_state_generation + 2,
            &[(
                QUALIFICATION_OUTPUT_SIZE.width / 2,
                QUALIFICATION_OUTPUT_SIZE.height / 2,
                remaining_expected,
            )],
            false,
            before_missing,
        )?;
        let before_late = renderer.presentation_evidence().upload_serials;
        renderer
            .upload_layer_rgba(
                &device,
                &queue,
                top_layer,
                QUALIFICATION_SOURCE_SIZE.width,
                QUALIFICATION_SOURCE_SIZE.height,
                &qualification_layer_rgba(top_layer).repeat(source_pixels),
            )
            .map_err(|error| format!("late qualification top-layer upload: {error}"))?;
        run_cross_adapter_state_scenario(
            &mut renderer,
            &device,
            &queue,
            &readback,
            &info,
            top_layer,
            &mut state_scenarios,
            "top_source_late_arrival",
            qualification_frame(layer_count),
            first_state_generation + 3,
            &[(
                QUALIFICATION_OUTPUT_SIZE.width / 2,
                QUALIFICATION_OUTPUT_SIZE.height / 2,
                expected,
            )],
            true,
            before_late,
        )?;
        Ok(CrossAdapterEvidence {
            name: info.name,
            vendor: info.vendor,
            device: info.device,
            device_type: format!("{:?}", info.device_type),
            backend: format!("{:?}", info.backend),
            driver: info.driver,
            driver_info: info.driver_info,
            timestamp_query_supported: true,
            layer_count: layer_count as u32,
            correctness_readback_passed: true,
            correctness_actual_rgba: actual,
            correctness_expected_rgba: expected,
            correctness_tolerance: QUALIFICATION_READBACK_TOLERANCE,
            warmup_submissions: QUALIFICATION_WARMUP_SUBMISSIONS,
            measured_submissions: QUALIFICATION_MEASURED_SUBMISSIONS,
            cpu_encode_timing,
            gpu_pass_timing,
            state_scenarios,
        })
    }

    #[test]
    fn gpu_timestamp_window_reports_supported_without_samples_and_bounded_p95() {
        let mut window = GpuTimestampWindow::default();
        assert_eq!(
            window.snapshot(true),
            ViewerCompositorGpuTiming {
                supported: true,
                ..Default::default()
            }
        );
        for milliseconds in 1..=COMPOSITOR_TIMING_SAMPLE_COUNT as u32 + 5 {
            window.push(milliseconds as f32);
        }
        assert_eq!(
            window.snapshot(true),
            ViewerCompositorGpuTiming {
                supported: true,
                samples: COMPOSITOR_TIMING_SAMPLE_COUNT,
                p95_ms: 119.0,
                max_ms: 125.0,
            }
        );
    }

    #[test]
    fn qualification_helpers_compute_expected_color_tolerance_and_p95() {
        assert_eq!(qualification_expected_rgba(2), [70, 128, 0, 255]);
        assert_eq!(qualification_expected_rgba(4), [90, 122, 112, 255]);
        assert!(rgba_within_tolerance(
            [91, 120, 116, 255],
            [90, 122, 112, 255],
            4
        ));
        assert!(!rgba_within_tolerance(
            [95, 123, 113, 255],
            [90, 122, 112, 255],
            4
        ));

        let mut samples = [5.0, 1.0, 3.0, 2.0, 4.0, 7.0, 6.0, 9.0, 8.0, 10.0];
        assert_eq!(
            timing_from_samples(&mut samples).expect("finite timing samples"),
            CrossAdapterTiming {
                samples: 10,
                p95_ms: 10.0,
                max_ms: 10.0,
            }
        );
    }

    #[test]
    fn timestamp_samples_reject_wrapped_zero_and_invalid_periods() {
        let sample = |start: u64, end: u64, period: f32| {
            let mut bytes = [0_u8; TIMESTAMP_RESULT_BYTES as usize];
            bytes[..8].copy_from_slice(&start.to_ne_bytes());
            bytes[8..].copy_from_slice(&end.to_ne_bytes());
            timestamp_sample_ms(&bytes, period)
        };
        assert_eq!(sample(10, 10, 1.0), None);
        assert_eq!(sample(10, 9, 1.0), None);
        assert_eq!(sample(10, 20, 0.0), None);
        assert_eq!(sample(10, 20, f32::NAN), None);
        assert_eq!(sample(10, 1_000_010, 1.0), Some(1.0));
    }

    #[test]
    fn gpu_timestamp_mailbox_allows_one_mapping_and_drains_once() {
        let mailbox = GpuTimestampMailbox::default();
        assert!(mailbox.reserve());
        assert!(!mailbox.reserve());
        assert!(mailbox.pending());
        mailbox.complete(true);
        assert_eq!(mailbox.take_completed(), Some(true));
        assert!(!mailbox.pending());
        assert_eq!(mailbox.take_completed(), None);
        assert!(mailbox.reserve());
        mailbox.complete(false);
        assert_eq!(mailbox.take_completed(), Some(false));
    }

    fn full_test_quad(id: u32) -> CompositeQuad {
        CompositeQuad {
            clip_id: ClipId(id),
            positions: [
                Point { x: 0.0, y: 0.0 },
                Point { x: 4.0, y: 0.0 },
                Point { x: 4.0, y: 4.0 },
                Point { x: 0.0, y: 4.0 },
            ],
            uvs: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            opacity: 1.0,
        }
    }

    #[test]
    fn viewer_shader_is_valid_current_wgsl() {
        naga::front::wgsl::parse_str(VIEWER_SHADER).expect("viewer WGSL");
    }

    #[test]
    fn viewer_sampling_quality_defaults_to_manual_bicubic() {
        assert_eq!(
            ViewerSamplingQuality::default(),
            ViewerSamplingQuality::Bicubic
        );
        assert_eq!(ViewerSamplingQuality::Nearest.shader_value(), 0);
        assert_eq!(ViewerSamplingQuality::Bilinear.shader_value(), 1);
        assert_eq!(ViewerSamplingQuality::Bicubic.shader_value(), 2);
    }

    #[test]
    fn sampling_change_requires_composition_without_a_frame_change() {
        let frame_generation = 17;
        let input_generation = 29;
        assert!(!composition_required(
            false,
            frame_generation,
            frame_generation,
            input_generation,
            input_generation,
        ));
        assert!(composition_required(
            false,
            frame_generation,
            frame_generation,
            input_generation,
            input_generation.wrapping_add(1),
        ));
    }

    #[test]
    fn viewer_shader_premultiplies_in_the_current_encoded_srgb_space() {
        assert!(VIEWER_SHADER.contains("fn fs_premultiply"));
        assert!(VIEWER_SHADER.contains("straight.rgb * straight.a"));
        assert!(VIEWER_SHADER.contains("sample.rgb / alpha"));
        assert!(VIEWER_SHADER.contains("encoded * output_alpha"));
        assert!(VIEWER_SHADER.contains("fn fs_blit_srgb"));
        assert!(VIEWER_SHADER.contains("fn fs_blit_encoded"));
        assert!(VIEWER_SHADER.contains("srgb_to_linear(sample.rgb)"));
        assert!(VIEWER_SHADER.contains("input.color * input.opacity"));
        assert!(VIEWER_SHADER.contains("fn texture_sample_bicubic"));
        assert!(VIEWER_SHADER.contains("textureLoad(source_texture, texel, 0)"));
        assert!(VIEWER_SHADER.contains("alpha_safe_premultiplied"));
    }

    #[test]
    fn compositor_pool_byte_estimates_include_all_retained_payloads() {
        let one_pixel = PixelSize::new(1, 1);
        let layer_payload = (std::mem::size_of::<GpuColorCorrectionStack>()
            + std::mem::size_of::<GpuCurveLutStack>()) as u64;
        assert_eq!(layer_resource_bytes(one_pixel), Some(8 + layer_payload));
        assert_eq!(
            output_pair_resource_bytes(one_pixel),
            Some(8 + 2 * layer_payload)
        );
        assert!(
            layer_resource_bytes(PixelSize::new(2_048, 2_048)).expect("4K-square layer accounting")
                > RESOURCE_POOL_BYTE_CAP
        );
        assert!(
            output_pair_resource_bytes(PixelSize::new(2_048, 2_048))
                .expect("4K-square output accounting")
                > RESOURCE_POOL_BYTE_CAP
        );
    }

    #[test]
    fn compositor_pool_cap_is_stricter_than_entry_caps() {
        let layer = layer_resource_bytes(PixelSize::new(1_920, 1_080)).expect("1080p layer");
        let output =
            output_pair_resource_bytes(PixelSize::new(1_920, 1_080)).expect("1080p outputs");
        assert!(layer * 2 <= RESOURCE_POOL_BYTE_CAP);
        assert!(layer * 2 + output > RESOURCE_POOL_BYTE_CAP);
        assert_eq!(MAX_POOLED_LAYER_BUNDLES, MAX_COMPOSITE_LAYERS);
        assert_eq!(MAX_POOLED_OUTPUT_PAIRS, 1);
    }

    #[test]
    fn blit_transfer_matches_the_presentation_format() {
        assert_eq!(
            blit_fragment_entry_point(wgpu::TextureFormat::Rgba8UnormSrgb),
            "fs_blit_srgb"
        );
        assert_eq!(
            blit_fragment_entry_point(wgpu::TextureFormat::Bgra8UnormSrgb),
            "fs_blit_srgb"
        );
        assert_eq!(
            blit_fragment_entry_point(wgpu::TextureFormat::Rgba8Unorm),
            "fs_blit_encoded"
        );
        assert_eq!(
            blit_fragment_entry_point(wgpu::TextureFormat::Bgra8Unorm),
            "fs_blit_encoded"
        );
    }

    #[test]
    fn compose_and_matte_use_premultiplied_source_over_blending() {
        let blend = premultiplied_blend();
        assert_eq!(blend.color.src_factor, wgpu::BlendFactor::One);
        assert_eq!(blend.color.dst_factor, wgpu::BlendFactor::OneMinusSrcAlpha);
        assert_eq!(blend.alpha.src_factor, wgpu::BlendFactor::One);
        assert_eq!(blend.alpha.dst_factor, wgpu::BlendFactor::OneMinusSrcAlpha);
    }

    #[test]
    fn callback_generations_only_advance_for_frame_changes() {
        let handle = ViewerCompositorCallbackHandle::new();
        assert_eq!(handle.generation(), 0);
        handle.clear();
        assert_eq!(handle.generation(), 0);
        handle.set_frame(ViewerFrame {
            project_size: PixelSize::new(1, 1),
            logical_canvas_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::ONE),
            black_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers: [None; MAX_COMPOSITE_LAYERS],
        });
        handle.clear();
        handle.clear();
        assert_eq!(handle.generation(), 2);
    }

    #[test]
    fn identical_frames_do_not_advance_the_composition_generation() {
        let handle = ViewerCompositorCallbackHandle::new();
        let frame = ViewerFrame {
            project_size: PixelSize::new(100, 50),
            logical_canvas_rect: egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(50.0, 25.0),
            ),
            black_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers: [None; MAX_COMPOSITE_LAYERS],
        };
        handle.set_frame(frame);
        handle.set_frame(frame);
        assert_eq!(handle.generation(), 1);
    }

    #[test]
    fn compositor_encode_timing_is_bounded_and_ignores_unrecorded_callbacks() {
        let mut state = CallbackState::default();
        assert_eq!(
            state.compositor_encode_timing(),
            ViewerCompositorEncodeTiming::default()
        );
        for milliseconds in 1..=COMPOSITOR_TIMING_SAMPLE_COUNT as u64 + 1 {
            state.record_compositor_encode(std::time::Duration::from_millis(milliseconds));
        }
        assert_eq!(
            state.compositor_encode_timing(),
            ViewerCompositorEncodeTiming {
                samples: COMPOSITOR_TIMING_SAMPLE_COUNT,
                p95_ms: 115.0,
                max_ms: 121.0,
            }
        );
    }

    #[test]
    fn live_compositor_timing_snapshot_never_waits_for_callback_lock() {
        let handle = ViewerCompositorCallbackHandle::new();
        assert_eq!(
            handle.try_compositor_encode_timing(),
            Some(ViewerCompositorEncodeTiming::default())
        );
        let _callback_guard = handle.state.lock().expect("callback state lock");
        assert_eq!(handle.try_compositor_encode_timing(), None);
    }

    #[test]
    fn presentation_evidence_tracks_upload_composition_and_blit_separately() {
        let mut tracker = ViewerPresentationTracker::default();
        tracker.record_upload(1);
        assert_eq!(
            tracker.evidence(),
            ViewerPresentationEvidence {
                upload_serials: [0, 1, 0, 0],
                ..Default::default()
            }
        );

        tracker.capture_composition([false, true, false, false]);
        assert_eq!(
            tracker.evidence().painted_upload_serials,
            [None; MAX_COMPOSITE_LAYERS]
        );
        tracker.record_paint();
        assert_eq!(
            tracker.evidence(),
            ViewerPresentationEvidence {
                upload_serials: [0, 1, 0, 0],
                painted_upload_serials: [None, Some(1), None, None],
                paint_serial: 1,
            }
        );
    }

    #[test]
    fn presentation_evidence_clears_current_identity_without_reusing_serials() {
        let mut tracker = ViewerPresentationTracker::default();
        tracker.record_upload(0);
        tracker.capture_composition([true, false, false, false]);
        tracker.record_paint();
        tracker.clear_layer(0);
        assert_eq!(tracker.evidence().upload_serials, [0; MAX_COMPOSITE_LAYERS]);
        // The prior blit remains historical evidence until a later blit supersedes it.
        assert_eq!(tracker.evidence().painted_upload_serials[0], Some(1));

        tracker.record_upload(0);
        assert_eq!(tracker.evidence().upload_serials[0], 2);
        tracker.clear();
        tracker.record_paint();
        assert_eq!(
            tracker.evidence(),
            ViewerPresentationEvidence {
                paint_serial: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn physical_canvas_size_uses_current_clamped_viewport_not_project_size() {
        let size = physical_canvas_size(
            egui::Rect::from_min_max(egui::pos2(10.2, 3.4), egui::pos2(90.7, 40.1)),
            1.5,
            [128, 72],
        );
        assert_eq!(size, PixelSize::new(113, 56));
        assert_eq!(
            physical_canvas_size(
                egui::Rect::from_min_size(egui::pos2(-20.0, -20.0), egui::vec2(2.0, 2.0)),
                1.0,
                [16, 16],
            ),
            PixelSize::new(1, 1)
        );
    }

    #[test]
    fn unchanged_frame_and_inputs_skip_composition() {
        assert!(!composition_required(false, 3, 3, 7, 7));
        assert!(composition_required(true, 3, 3, 7, 7));
        assert!(composition_required(false, 3, 4, 7, 7));
        assert!(composition_required(false, 3, 3, 7, 8));
    }

    #[test]
    fn upload_validation_rejects_invalid_slots_and_lengths() {
        assert_eq!(
            validate_upload(4, 1, 1, 4),
            Err(ViewerUploadError::LayerOutOfBounds { layer: 4 })
        );
        assert_eq!(
            validate_upload(0, 2, 2, 3),
            Err(ViewerUploadError::InvalidRgbaLength {
                expected: 16,
                actual: 3
            })
        );
        assert_eq!(
            validate_upload(0, 0, 2, 0),
            Err(ViewerUploadError::ZeroDimension)
        );
    }

    #[test]
    fn vertices_preserve_quad_order_crop_and_content_uv() {
        let quad = CompositeQuad {
            clip_id: ClipId(1),
            positions: [
                Point { x: 0.0, y: 0.0 },
                Point { x: 100.0, y: 0.0 },
                Point { x: 100.0, y: 100.0 },
                Point { x: 0.0, y: 100.0 },
            ],
            uvs: [
                Uv { u: 0.25, v: 0.0 },
                Uv { u: 0.75, v: 0.0 },
                Uv { u: 0.75, v: 1.0 },
                Uv { u: 0.25, v: 1.0 },
            ],
            opacity: 0.5,
        };
        let mut vertices = [ViewerVertex::zeroed(); VERTICES_PER_LAYER];
        write_vertices(
            &mut vertices,
            ViewerLayerPrimitive {
                quad,
                content_uv: [
                    Uv { u: 0.1, v: 0.2 },
                    Uv { u: 0.9, v: 0.2 },
                    Uv { u: 0.9, v: 0.8 },
                    Uv { u: 0.1, v: 0.8 },
                ],
                color_corrections: [ViewerColorCorrection::default();
                    MAX_COLOR_CORRECTIONS_PER_LAYER],
                color_correction_count: 0,
            },
            PixelSize::new(100, 100),
        );
        assert_eq!(vertices[0].position, [-1.0, 1.0]);
        assert!((vertices[0].uv[0] - 0.3).abs() < 0.0001);
        assert!((vertices[1].uv[0] - 0.7).abs() < 0.0001);
        assert_eq!(vertices[0].opacity, 0.5);
        let stack = gpu_color_correction_stack(ViewerLayerPrimitive {
            quad,
            content_uv: [Uv { u: 0.0, v: 0.0 }; 4],
            color_corrections: [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER],
            color_correction_count: 0,
        });
        assert_eq!(stack.count, 0);
    }

    #[test]
    fn uniform_packs_ordered_basic_corrections_and_sanitizes_active_slots() {
        let mut corrections = [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER];
        corrections[0] = ViewerColorCorrection {
            temperature: 0.5,
            tint: -0.25,
            saturation: 1.5,
            exposure: 2.0,
            brightness: 0.25,
            contrast: 2.0,
            highlights: 0.75,
            shadows: -0.5,
            whites: 0.6,
            blacks: -0.7,
            ..Default::default()
        };
        corrections[1] = ViewerColorCorrection {
            brightness: -2.0,
            contrast: f32::NAN,
            ..Default::default()
        };
        corrections[2] = ViewerColorCorrection {
            brightness: f32::INFINITY,
            contrast: 99.0,
            ..Default::default()
        };
        corrections[3] = ViewerColorCorrection {
            brightness: 0.75,
            contrast: 0.5,
            ..Default::default()
        };
        let primitive = ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            color_corrections: corrections,
            color_correction_count: 3,
        };
        let stack = gpu_color_correction_stack(primitive);
        let curves = gpu_curve_lut_stack(primitive);
        assert_eq!(stack.count, 3);
        assert_eq!(stack.corrections[0].color, [0.5, -0.25, 1.5, 2.0]);
        assert_eq!(stack.corrections[0].light, [0.25, 2.0, 0.75, -0.5]);
        assert_eq!(stack.corrections[0].center, [0.0, 0.0, 0.6, -0.7]);
        assert_eq!(stack.corrections[0].effect[0], 0.0);
        assert_eq!(curves.samples[255], [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(
            stack.corrections.map(|correction| correction.light),
            [
                [0.25, 2.0, 0.75, -0.5],
                [-1.0, 1.0, 0.0, 0.0],
                [0.0, 4.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
            ]
        );
    }

    #[test]
    fn vignette_payload_is_sanitized_and_darkens_edges_but_not_its_center() {
        let vignette = ViewerColorCorrection {
            operation: ViewerColorCorrectionOperation::Vignette,
            vignette_amount: 2.0,
            vignette_midpoint: f32::NAN,
            vignette_feather: 0.5,
            vignette_center_x: f32::INFINITY,
            vignette_center_y: -2.0,
            ..Default::default()
        };
        let stack = gpu_color_correction_stack(ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [Uv { u: 0.0, v: 0.0 }; 4],
            color_corrections: [vignette; MAX_COLOR_CORRECTIONS_PER_LAYER],
            color_correction_count: 1,
        });
        assert_eq!(stack.corrections[0].effect, [1.0, 1.0, 0.0, 0.5]);
        assert_eq!(stack.corrections[0].center, [0.0, -1.0, 0.0, 0.0]);

        let center = vignette_multiplier([0.5, 0.5], 1.0, 0.5, 0.5, 0.0, 0.0);
        let edge = vignette_multiplier([1.0, 1.0], 1.0, 0.5, 0.5, 0.0, 0.0);
        assert_eq!(center, 1.0);
        assert!(edge < center);
    }

    #[test]
    fn basic_correction_reuses_center_tail_for_sanitized_whites_and_blacks() {
        let correction = ViewerColorCorrection {
            whites: f32::INFINITY,
            blacks: -2.0,
            ..Default::default()
        };
        let stack = gpu_color_correction_stack(ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [Uv { u: 0.0, v: 0.0 }; 4],
            color_corrections: [correction; MAX_COLOR_CORRECTIONS_PER_LAYER],
            color_correction_count: 1,
        });
        assert_eq!(stack.corrections[0].center, [0.0, 0.0, 0.0, -1.0]);
        assert_eq!(std::mem::size_of::<GpuColorCorrection>(), 64);
        assert!(VIEWER_SHADER.contains("let tonal_luma = clamp(luma, 0.0, 1.0);"));
        assert!(VIEWER_SHADER.contains("pow(tonal_luma, 8.0)"));
        assert!(VIEWER_SHADER.contains("pow(1.0 - tonal_luma, 8.0)"));
    }

    #[test]
    fn malformed_curve_falls_back_to_identity_without_touching_inactive_slots() {
        let mut corrections = [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER];
        corrections[0].curves.red.count = 3;
        corrections[0].curves.red.points[1] = [0.001, 0.5];
        corrections[0].curves.red.points[2] = [1.0, 1.0];
        corrections[1].curves.master.points[1] = [1.0, 0.25];
        let stack = gpu_curve_lut_stack(ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [Uv { u: 0.0, v: 0.0 }; 4],
            color_corrections: corrections,
            color_correction_count: 1,
        });
        let identity = 16.0 / 255.0;
        assert_eq!(stack.samples[16], [identity, identity, identity, 0.0]);
        assert_eq!(
            stack.samples[COLOR_CURVE_LUT_SAMPLES + 16],
            [identity, identity, identity, 0.0]
        );
    }

    #[test]
    fn curve_storage_preserves_adjacent_8_bit_control_levels() {
        let mut corrections = [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER];
        corrections[0].curves.red.count = 4;
        corrections[0].curves.red.points[0] = [0.0, 0.0];
        corrections[0].curves.red.points[1] = [1.0 / 255.0, 1.0];
        corrections[0].curves.red.points[2] = [2.0 / 255.0, 0.0];
        corrections[0].curves.red.points[3] = [1.0, 1.0];
        let stack = gpu_curve_lut_stack(ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [Uv { u: 0.0, v: 0.0 }; 4],
            color_corrections: corrections,
            color_correction_count: 1,
        });
        assert_eq!(stack.samples[0][0], 0.0);
        assert_eq!(stack.samples[1][0], 1.0);
        assert_eq!(stack.samples[2][0], 0.0);
    }

    #[test]
    fn vertices_clamp_color_correction_count_to_payload_capacity() {
        let correction = ViewerColorCorrection {
            brightness: 0.25,
            contrast: 2.0,
            ..Default::default()
        };
        let primitive = ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            color_corrections: [correction; MAX_COLOR_CORRECTIONS_PER_LAYER],
            color_correction_count: u32::MAX,
        };
        let stack = gpu_color_correction_stack(primitive);
        assert_eq!(stack.count, MAX_COLOR_CORRECTIONS_PER_LAYER as u32);
        assert_eq!(
            stack.corrections.map(|correction| correction.light),
            [[0.25, 2.0, 0.0, 0.0]; MAX_COLOR_CORRECTIONS_PER_LAYER]
        );
    }

    #[test]
    fn black_matte_opacity_is_finite_clamped_and_part_of_frame_identity() {
        assert_eq!(sanitize_matte_opacity(-1.0), 0.0);
        assert_eq!(sanitize_matte_opacity(0.25), 0.25);
        assert_eq!(sanitize_matte_opacity(2.0), 1.0);
        assert_eq!(sanitize_matte_opacity(f32::NAN), 0.0);
        assert_eq!(
            matte_vertices(
                [f32::NAN, -1.0, 0.25, 2.0, 1.0],
                [0.0; MAX_COMPOSITE_LAYERS + 1],
            )
            .map(|vertex| vertex.opacity),
            [0.0, 0.0, 0.25, 1.0, 1.0]
        );
        let white = matte_vertices(
            [0.0; MAX_COMPOSITE_LAYERS + 1],
            [0.0, 0.25, 1.0, 2.0, f32::NAN],
        );
        assert_eq!(
            white.map(|vertex| vertex.opacity),
            [0.0, 0.25, 1.0, 1.0, 0.0]
        );
        assert_eq!(white[1].color, [1.0; 3]);

        let frame = ViewerFrame {
            project_size: PixelSize::new(4, 4),
            logical_canvas_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(4.0, 4.0)),
            black_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers: [None; MAX_COMPOSITE_LAYERS],
        };
        let handle = ViewerCompositorCallbackHandle::new();
        handle.set_frame(frame);
        handle.set_frame(ViewerFrame {
            black_mattes_before: [0.0, 0.0, 1.0, 0.0, 0.0],
            ..frame
        });
        assert_eq!(handle.generation(), 2);
    }

    #[test]
    fn color_correction_stack_changes_retained_frame_generation_and_vertex_data() {
        let mut corrections = [ViewerColorCorrection::default(); MAX_COLOR_CORRECTIONS_PER_LAYER];
        corrections[0] = ViewerColorCorrection {
            brightness: 0.1,
            contrast: 1.5,
            ..Default::default()
        };
        corrections[1] = ViewerColorCorrection {
            brightness: -0.25,
            contrast: 2.5,
            ..Default::default()
        };
        let primitive = ViewerLayerPrimitive {
            quad: full_test_quad(1),
            content_uv: [
                Uv { u: 0.0, v: 0.0 },
                Uv { u: 1.0, v: 0.0 },
                Uv { u: 1.0, v: 1.0 },
                Uv { u: 0.0, v: 1.0 },
            ],
            color_corrections: corrections,
            color_correction_count: 2,
        };
        let frame = |layer| ViewerFrame {
            project_size: PixelSize::new(4, 4),
            logical_canvas_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(4.0, 4.0)),
            black_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers: [Some(layer), None, None, None],
        };
        let handle = ViewerCompositorCallbackHandle::new();
        handle.set_frame(frame(primitive));
        let generation_before_correction = handle.generation();
        let corrected = ViewerLayerPrimitive {
            color_corrections: [
                corrections[1],
                corrections[0],
                ViewerColorCorrection::default(),
                ViewerColorCorrection::default(),
                ViewerColorCorrection::default(),
                ViewerColorCorrection::default(),
                ViewerColorCorrection::default(),
                ViewerColorCorrection::default(),
            ],
            ..primitive
        };
        handle.set_frame(frame(corrected));
        assert_eq!(handle.generation(), generation_before_correction + 1);

        let stack = gpu_color_correction_stack(corrected);
        assert_eq!(stack.count, 2);
        assert_eq!(stack.corrections[0].light, [-0.25, 2.5, 0.0, 0.0]);
        assert_eq!(stack.corrections[1].light, [0.1, 1.5, 0.0, 0.0]);
    }

    #[test]
    #[ignore = "requires a real GPU adapter; run with --ignored --nocapture"]
    fn gpu_bicubic_filters_premultiplied_edges_without_dark_halos() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("viewer GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("premultiplied edge test device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("premultiplied edge test device");
        let mut renderer =
            ViewerCompositorRenderer::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(renderer.sampling_quality(), ViewerSamplingQuality::Bicubic);
        renderer
            .upload_layer_rgba(&device, &queue, 0, 2, 1, &[255, 0, 0, 255, 0, 0, 0, 0])
            .expect("straight-alpha edge upload");
        let frame = ViewerFrame {
            project_size: PixelSize::new(1, 1),
            logical_canvas_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::ONE),
            black_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            white_mattes_before: [0.0; MAX_COMPOSITE_LAYERS + 1],
            layers: [
                Some(ViewerLayerPrimitive {
                    quad: CompositeQuad {
                        clip_id: ClipId(1),
                        positions: [
                            Point { x: 0.0, y: 0.0 },
                            Point { x: 1.0, y: 0.0 },
                            Point { x: 1.0, y: 1.0 },
                            Point { x: 0.0, y: 1.0 },
                        ],
                        uvs: [
                            Uv { u: 0.0, v: 0.0 },
                            Uv { u: 1.0, v: 0.0 },
                            Uv { u: 1.0, v: 1.0 },
                            Uv { u: 0.0, v: 1.0 },
                        ],
                        opacity: 1.0,
                    },
                    content_uv: [
                        Uv { u: 0.0, v: 0.0 },
                        Uv { u: 1.0, v: 0.0 },
                        Uv { u: 1.0, v: 1.0 },
                        Uv { u: 0.0, v: 1.0 },
                    ],
                    color_corrections: [ViewerColorCorrection::default();
                        MAX_COLOR_CORRECTIONS_PER_LAYER],
                    color_correction_count: 0,
                }),
                None,
                None,
                None,
            ],
        };
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("premultiplied edge readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        assert!(renderer.prepare(
            &device,
            &queue,
            &mut encoder,
            Some(frame),
            1,
            PixelSize::new(1, 1),
        ));
        let output = &renderer.outputs.as_ref().expect("output after prepare")
            [renderer.front_output]
            ._texture;
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: output,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let pixel = readback_first_pixel(&device, &readback, queue.submit([encoder.finish()]))
            .expect("premultiplied edge readback");
        assert!(
            (125..=130).contains(&pixel[0]) && pixel[1] <= 1 && pixel[2] <= 1 && pixel[3] == 255,
            "filtered straight edge must contribute half-strength red over black, got {pixel:?}"
        );

        let upload_serials = renderer.presentation_evidence().upload_serials;
        renderer.set_sampling_quality(ViewerSamplingQuality::Bilinear);
        let mut resample_encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        assert!(renderer.prepare(
            &device,
            &queue,
            &mut resample_encoder,
            Some(frame),
            1,
            PixelSize::new(1, 1),
        ));
        queue.submit([resample_encoder.finish()]);
        assert_eq!(
            renderer.presentation_evidence().upload_serials,
            upload_serials
        );
        let mut unchanged_encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        assert!(!renderer.prepare(
            &device,
            &queue,
            &mut unchanged_encoder,
            Some(frame),
            1,
            PixelSize::new(1, 1),
        ));

        let display = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("encoded edge presentation target"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let display_view = display.create_view(&Default::default());
        let display_readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("encoded edge presentation readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("encoded edge presentation pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &display_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&renderer.blit_pipeline);
            pass.set_bind_group(
                0,
                &renderer.outputs.as_ref().expect("composed output")[renderer.front_output]
                    .blit_bind_group,
                &[],
            );
            pass.draw(0..VERTICES_PER_LAYER as u32, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &display,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &display_readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let displayed =
            readback_first_pixel(&device, &display_readback, queue.submit([encoder.finish()]))
                .expect("encoded edge presentation readback");
        assert!(
            (125..=130).contains(&displayed[0])
                && displayed[1] <= 1
                && displayed[2] <= 1
                && displayed[3] == 255,
            "sRGB presentation changed encoded compositor energy: {displayed:?}"
        );

        let mut unorm_renderer =
            ViewerCompositorRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        unorm_renderer
            .upload_layer_rgba(&device, &queue, 0, 2, 1, &[255, 0, 0, 255, 0, 0, 0, 0])
            .expect("straight-alpha edge upload for non-sRGB fallback");
        let unorm_display = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("encoded edge non-sRGB presentation target"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let unorm_display_view = unorm_display.create_view(&Default::default());
        let unorm_readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("encoded edge non-sRGB presentation readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        assert!(unorm_renderer.prepare(
            &device,
            &queue,
            &mut encoder,
            Some(frame),
            1,
            PixelSize::new(1, 1),
        ));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("encoded edge non-sRGB presentation pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &unorm_display_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&unorm_renderer.blit_pipeline);
            pass.set_bind_group(
                0,
                &unorm_renderer.outputs.as_ref().expect("composed output")
                    [unorm_renderer.front_output]
                    .blit_bind_group,
                &[],
            );
            pass.draw(0..VERTICES_PER_LAYER as u32, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &unorm_display,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &unorm_readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let unorm_displayed =
            readback_first_pixel(&device, &unorm_readback, queue.submit([encoder.finish()]))
                .expect("non-sRGB presentation readback");
        assert!(
            (125..=130).contains(&unorm_displayed[0])
                && unorm_displayed[1] <= 1
                && unorm_displayed[2] <= 1
                && unorm_displayed[3] == 255,
            "non-sRGB fallback changed encoded compositor energy: {unorm_displayed:?}"
        );
    }

    #[test]
    #[ignore = "requires a real GPU adapter; run with --ignored --nocapture"]
    fn gpu_interleaves_black_matte_between_media_layers() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("viewer GPU adapter");
        let timestamp_query_supported =
            adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        if std::env::var_os("MAELSTROM_REQUIRE_GPU_TIMESTAMP_QUERY").is_some() {
            assert!(
                timestamp_query_supported,
                "selected adapter must support TIMESTAMP_QUERY for the explicit timing gate"
            );
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("viewer compositor black-matte test device"),
            required_features: if timestamp_query_supported {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            },
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("viewer compositor black-matte device");
        let mut renderer = ViewerCompositorRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        renderer.set_gpu_timing_enabled(timestamp_query_supported);
        renderer
            .upload_layer_rgba(&device, &queue, 0, 4, 4, &[255, 0, 0, 255].repeat(16))
            .expect("upload red input");
        renderer
            .upload_layer_rgba(&device, &queue, 1, 4, 4, &[255, 255, 255, 255].repeat(16))
            .expect("upload white input");
        let frame = black_matte_qualification_frame();
        for generation in 1..=2 {
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewer compositor black-matte readback"),
                size: 256 * 4,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            assert!(renderer.prepare(
                &device,
                &queue,
                &mut encoder,
                Some(frame),
                generation,
                PixelSize::new(4, 4),
            ));
            assert!(!renderer.prepare(
                &device,
                &queue,
                &mut encoder,
                Some(frame),
                generation,
                PixelSize::new(4, 4),
            ));
            let output = &renderer.outputs.as_ref().expect("output after prepare")
                [renderer.front_output]
                ._texture;
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: output,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(4),
                    },
                },
                wgpu::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
            );
            verify_black_matte_readback(&device, &readback, queue.submit([encoder.finish()]))
                .expect("black-matte compositor readback");
            renderer.drain_gpu_timing(&queue);
        }
        if timestamp_query_supported {
            assert_eq!(renderer.gpu_timing().samples, 2);
        }
    }

    #[test]
    #[ignore = "requires a real GPU adapter; run with --ignored --nocapture"]
    fn gpu_compositor_reuses_exact_size_resources_and_rejects_oversize_pool_entries() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("viewer GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("viewer compositor pool test device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("viewer GPU device");
        let mut renderer = ViewerCompositorRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        let mut frame = qualification_frame(2);
        frame.black_mattes_before = [0.0; MAX_COMPOSITE_LAYERS + 1];
        frame.layers[1] = None;
        frame.layers[0]
            .as_mut()
            .expect("pool test layer")
            .color_correction_count = 0;
        let encode = |renderer: &mut ViewerCompositorRenderer, generation, canvas_size| {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer compositor pool test encoder"),
            });
            assert!(renderer.prepare(
                &device,
                &queue,
                &mut encoder,
                Some(frame),
                generation,
                canvas_size,
            ));
            queue.submit([encoder.finish()])
        };

        renderer
            .upload_layer_rgba(&device, &queue, 0, 8, 8, &[255, 0, 0, 255].repeat(64))
            .expect("initial layer upload");
        renderer
            .upload_layer_rgba(&device, &queue, 0, 16, 16, &[255, 0, 0, 255].repeat(256))
            .expect("resized layer upload");
        renderer
            .upload_layer_rgba(&device, &queue, 0, 8, 8, &[255, 0, 0, 255].repeat(64))
            .expect("exact-size layer reuse upload");
        let _ = encode(&mut renderer, 1, PixelSize::new(8, 8));
        let _ = encode(&mut renderer, 2, PixelSize::new(16, 16));
        let _ = encode(&mut renderer, 3, PixelSize::new(8, 8));
        renderer.clear_layer(0).expect("pool cleared layer");
        renderer
            .upload_layer_rgba(&device, &queue, 0, 8, 8, &[255, 0, 0, 255].repeat(64))
            .expect("reuse layer after clear");
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("viewer compositor invalid-frame pool encoder"),
        });
        assert!(!renderer.prepare(&device, &queue, &mut encoder, None, 4, PixelSize::new(8, 8),));
        queue.submit([encoder.finish()]);
        let _ = encode(&mut renderer, 5, PixelSize::new(8, 8));

        let oversized = vec![0_u8; 2_048 * 2_048 * 4];
        renderer
            .upload_layer_rgba(&device, &queue, 0, 2_048, 2_048, &oversized)
            .expect("oversize layer upload");
        renderer
            .upload_layer_rgba(&device, &queue, 0, 8, 8, &[255, 0, 0, 255].repeat(64))
            .expect("release oversize layer upload");
        let _ = encode(&mut renderer, 6, PixelSize::new(2_048, 2_048));
        let _ = encode(&mut renderer, 7, PixelSize::new(8, 8));

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer compositor pooled output readback"),
            size: 256 * 8,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("viewer compositor pooled output readback encoder"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &renderer.outputs.as_ref().expect("pooled outputs allocated")
                    [renderer.front_output]
                    ._texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(8),
                },
            },
            wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
        );
        let pixel = readback_first_pixel(&device, &readback, queue.submit([encoder.finish()]))
            .expect("pooled output readback after queued submissions");
        assert!(
            pixel[0] >= 200 && pixel[1] <= 2 && pixel[2] <= 2 && pixel[3] == 255,
            "pooled queued submissions changed the final red frame: {pixel:?}"
        );

        let counters = renderer.pool_counters();
        assert!(
            counters.layer_reuses >= 2,
            "exact-size layer reuse was not observed: {counters:?}"
        );
        assert!(
            counters.output_reuses >= 1,
            "exact-size output-pair reuse was not observed: {counters:?}"
        );
        assert!(
            counters.rejected_oversize >= 2,
            "oversize layer/output resources entered the pool: {counters:?}"
        );
        assert!(
            counters.evictions >= 1,
            "single output-pair pool did not deterministically evict: {counters:?}"
        );
        assert!(renderer.pooled_bytes <= RESOURCE_POOL_BYTE_CAP);
        renderer.clear();
        assert_eq!(renderer.pooled_bytes, 0);
        assert!(renderer.layer_pool.iter().all(Option::is_none));
        assert!(renderer.output_pool.is_none());
    }

    #[test]
    #[ignore = "requires DX12 integrated and discrete adapters plus MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT"]
    fn phase0_cross_adapter_viewer_compositor_qualification() {
        let report_path = std::env::var(CROSS_ADAPTER_REPORT_ENV).unwrap_or_else(|_| {
            panic!("{CROSS_ADAPTER_REPORT_ENV} must name an absolute JSON report path")
        });
        let report_path = Path::new(&report_path);
        assert!(
            report_path.is_absolute(),
            "{CROSS_ADAPTER_REPORT_ENV} must name an absolute JSON report path"
        );
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
        let available = adapters
            .iter()
            .map(|adapter| adapter_label(&adapter.get_info()))
            .collect::<Vec<_>>();
        let mut evidence = Vec::with_capacity(2);
        for required_type in [
            wgpu::DeviceType::IntegratedGpu,
            wgpu::DeviceType::DiscreteGpu,
        ] {
            let adapter = adapters
                .iter()
                .find(|adapter| adapter.get_info().device_type == required_type)
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "required DX12 {required_type:?} adapter is unavailable; enumerated: {}",
                        available.join("; ")
                    )
                });
            evidence.push(
                qualify_viewer_compositor_adapter(
                    adapter,
                    if required_type == wgpu::DeviceType::IntegratedGpu {
                        2
                    } else {
                        4
                    },
                )
                .unwrap_or_else(|error| {
                    panic!("DX12 {required_type:?} viewer compositor qualification failed: {error}")
                }),
            );
        }
        let report = CrossAdapterReport {
            schema_version: 3,
            status: "passed",
            scope: "headless_transformed_multilayer_viewer_compositor_with_post_measurement_state_scenarios",
            physical_scanout_observed: false,
            app_auto_preview_observed: false,
            machine: cross_adapter_machine(),
            workload: CrossAdapterWorkload {
                source_width: QUALIFICATION_SOURCE_SIZE.width,
                source_height: QUALIFICATION_SOURCE_SIZE.height,
                output_width: QUALIFICATION_OUTPUT_SIZE.width,
                output_height: QUALIFICATION_OUTPUT_SIZE.height,
                sampling: "Bicubic",
                warmup_submissions: QUALIFICATION_WARMUP_SUBMISSIONS,
                measured_submissions: QUALIFICATION_MEASURED_SUBMISSIONS,
                target_fps: 30,
                frame_budget_ms: QUALIFICATION_FRAME_BUDGET_MS,
                uploads_excluded_from_timing: true,
                warmup_excluded_from_timing: true,
            },
            adapters: evidence,
        };
        write_cross_adapter_report(report_path, &report)
            .unwrap_or_else(|error| panic!("write cross-adapter report: {error}"));
    }
}
