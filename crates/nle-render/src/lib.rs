//! GPU rendering primitives for Maelstrom.  This crate performs no runtime I/O.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Instant,
};

use bytemuck::{Pod, Zeroable};
use glam::{
    Mat4, Vec3,
    camera::rh::{proj::directx, view},
};
use image::ImageFormat;
use wgpu::util::DeviceExt;

mod rect_renderer;
mod texture_renderer;
mod viewer_compositor;

pub use rect_renderer::{
    PixelViewport, RectInstance, RectRenderRegion, ScissorRect, TimelineRectCallbackHandle,
    TimelineRectRenderer,
};
pub use texture_renderer::{
    TextureInstance, TextureUploadError, TexturedRect, TimelineTextureCallbackHandle,
    TimelineTextureRenderer,
};
pub use viewer_compositor::{
    MAX_COLOR_CORRECTIONS_PER_LAYER, ViewerColorCorrection, ViewerColorCurve,
    ViewerCompositorCallbackHandle, ViewerCompositorEncodeTiming, ViewerCompositorRenderer,
    ViewerFrame, ViewerLayerPrimitive, ViewerRgbCurves, ViewerUploadError,
};

const GPU_COMPLETION_SAMPLE_WINDOW: usize = 120;

/// Snapshot of submission-to-GPU-completion elapsed time.
///
/// This is CPU monotonic elapsed time from immediately before a queue submission
/// until wgpu reports that submission has finished on the GPU. It includes queue
/// backlog and driver scheduling; it is not isolated GPU pass execution time or
/// display scanout time. Callback dispatch and non-blocking poll cadence may
/// also extend the observed elapsed time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuSubmissionCompletionTiming {
    pub samples: usize,
    pub p95_ms: f32,
    pub max_ms: f32,
}

#[derive(Debug)]
struct GpuCompletionTimingWindow {
    samples: [u64; GPU_COMPLETION_SAMPLE_WINDOW],
    len: usize,
    next: usize,
}

impl Default for GpuCompletionTimingWindow {
    fn default() -> Self {
        Self {
            samples: [0; GPU_COMPLETION_SAMPLE_WINDOW],
            len: 0,
            next: 0,
        }
    }
}

impl GpuCompletionTimingWindow {
    fn push(&mut self, elapsed_nanos: u64) {
        self.samples[self.next] = elapsed_nanos;
        self.next = (self.next + 1) % GPU_COMPLETION_SAMPLE_WINDOW;
        self.len = (self.len + 1).min(GPU_COMPLETION_SAMPLE_WINDOW);
    }

    fn snapshot(&self) -> GpuSubmissionCompletionTiming {
        if self.len == 0 {
            return GpuSubmissionCompletionTiming::default();
        }

        let mut ordered = self.samples;
        ordered[..self.len].sort_unstable();
        let p95_index = (self.len * 95).div_ceil(100).saturating_sub(1);
        GpuSubmissionCompletionTiming {
            samples: self.len,
            p95_ms: ordered[p95_index] as f32 / 1_000_000.0,
            max_ms: ordered[self.len - 1] as f32 / 1_000_000.0,
        }
    }
}

/// One callback/sample may be outstanding. The callback only publishes elapsed
/// nanoseconds and completion state; the render thread owns window mutation.
#[derive(Debug, Default)]
struct GpuCompletionMailbox {
    // 0 = idle, 1 = callback pending, 2 = completed sample ready to drain.
    state: AtomicU8,
    elapsed_nanos: AtomicU64,
}

impl GpuCompletionMailbox {
    fn reserve(&self) -> Option<Instant> {
        self.state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Instant::now())
    }

    fn complete(&self, elapsed_nanos: u64) {
        self.elapsed_nanos.store(elapsed_nanos, Ordering::Relaxed);
        self.state.store(2, Ordering::Release);
    }

    fn drain_completed(&self) -> Option<u64> {
        self.state
            .compare_exchange(2, 0, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| self.elapsed_nanos.load(Ordering::Relaxed))
    }
}

/// Thin egui GPU submission adapter; application code retains window/event ownership.
pub struct HubRenderer {
    renderer: egui_wgpu::Renderer,
    timeline_rects: TimelineRectCallbackHandle,
    timeline_textures: TimelineTextureCallbackHandle,
    viewer_compositor: ViewerCompositorCallbackHandle,
    gpu_completion_mailbox: Arc<GpuCompletionMailbox>,
    gpu_completion_timing: GpuCompletionTimingWindow,
}

impl HubRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let mut renderer = egui_wgpu::Renderer::new(device, format, Default::default());
        renderer
            .callback_resources
            .insert(TimelineRectRenderer::new(device, format));
        renderer
            .callback_resources
            .insert(TimelineTextureRenderer::new(device, format));
        renderer
            .callback_resources
            .insert(ViewerCompositorRenderer::new(device, format));
        Self {
            renderer,
            timeline_rects: TimelineRectCallbackHandle::new(),
            timeline_textures: TimelineTextureCallbackHandle::new(),
            viewer_compositor: ViewerCompositorCallbackHandle::new(),
            gpu_completion_mailbox: Arc::new(GpuCompletionMailbox::default()),
            gpu_completion_timing: GpuCompletionTimingWindow::default(),
        }
    }

    /// Shared retained input for one native timeline rectangle paint callback.
    pub fn timeline_rects(&self) -> TimelineRectCallbackHandle {
        self.timeline_rects.clone()
    }

    /// Shared retained input for native timeline thumbnail atlas cells.
    pub fn timeline_textures(&self) -> TimelineTextureCallbackHandle {
        self.timeline_textures.clone()
    }

    /// Shared retained input for the native project-monitor compositor.
    pub fn viewer_compositor(&self) -> ViewerCompositorCallbackHandle {
        self.viewer_compositor.clone()
    }

    /// Snapshot CPU command-encoding time for changed viewer compositions only.
    pub fn viewer_compositor_encode_timing(&self) -> ViewerCompositorEncodeTiming {
        self.viewer_compositor.compositor_encode_timing()
    }

    /// Snapshot bounded submission-to-GPU-completion timing samples.
    pub fn gpu_submission_completion_timing(&self) -> GpuSubmissionCompletionTiming {
        self.gpu_completion_timing.snapshot()
    }

    /// Uploads a decoded RGBA frame into one fixed project-monitor layer slot.
    pub fn upload_viewer_layer_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layer: usize,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), ViewerUploadError> {
        self.renderer
            .callback_resources
            .get_mut::<ViewerCompositorRenderer>()
            .expect("viewer compositor renderer is registered")
            .upload_layer_rgba(device, queue, layer, width, height, rgba)
    }

    /// Releases one decoded project-monitor layer slot.
    pub fn clear_viewer_layer(&mut self, layer: usize) -> Result<(), ViewerUploadError> {
        self.renderer
            .callback_resources
            .get_mut::<ViewerCompositorRenderer>()
            .expect("viewer compositor renderer is registered")
            .clear_layer(layer)
    }

    /// Releases all project-monitor GPU inputs and retained callback state.
    pub fn clear_viewer_compositor(&mut self) {
        if let Some(renderer) = self
            .renderer
            .callback_resources
            .get_mut::<ViewerCompositorRenderer>()
        {
            renderer.clear();
        }
        self.viewer_compositor.clear();
    }

    pub fn upload_timeline_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture_id: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), TextureUploadError> {
        self.renderer
            .callback_resources
            .get_mut::<TimelineTextureRenderer>()
            .expect("timeline texture renderer is registered")
            .upload_rgba(device, queue, texture_id, width, height, rgba)
    }

    pub fn clear_timeline_textures(&mut self) {
        if let Some(renderer) = self
            .renderer
            .callback_resources
            .get_mut::<TimelineTextureRenderer>()
        {
            renderer.clear_textures();
        }
        self.timeline_textures.clear();
    }

    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
        screen: egui_wgpu::ScreenDescriptor,
    ) {
        self.render_with_load(
            device,
            queue,
            view,
            primitives,
            textures_delta,
            screen,
            wgpu::LoadOp::Clear(wgpu::Color::BLACK),
            "project hub encoder",
            "project hub pass",
            false,
        );
    }

    /// Renders one frame and observes its queue submission completing on the GPU.
    ///
    /// This opt-in path is reserved for the bounded surface performance report.
    pub fn render_with_gpu_completion_measurement(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
        screen: egui_wgpu::ScreenDescriptor,
    ) {
        self.render_with_load(
            device,
            queue,
            view,
            primitives,
            textures_delta,
            screen,
            wgpu::LoadOp::Clear(wgpu::Color::BLACK),
            "project hub encoder",
            "project hub pass",
            true,
        );
    }

    /// Composites egui over an already-drawn splash without clearing the cylinder.
    pub fn render_overlay(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
        screen: egui_wgpu::ScreenDescriptor,
    ) {
        self.render_with_load(
            device,
            queue,
            view,
            primitives,
            textures_delta,
            screen,
            wgpu::LoadOp::Load,
            "splash overlay encoder",
            "splash overlay pass",
            false,
        );
    }

    fn render_with_load(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
        screen: egui_wgpu::ScreenDescriptor,
        load: wgpu::LoadOp<wgpu::Color>,
        encoder_label: &'static str,
        pass_label: &'static str,
        measure_gpu_completion: bool,
    ) {
        if measure_gpu_completion {
            // Poll, never wait: callbacks are serviced opportunistically without
            // stalling the interactive render path.
            let _ = device.poll(wgpu::PollType::Poll);
            if let Some(elapsed_nanos) = self.gpu_completion_mailbox.drain_completed() {
                self.gpu_completion_timing.push(elapsed_nanos);
            }
        }
        for (id, delta) in &textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some(encoder_label),
        });
        self.renderer
            .update_buffers(device, queue, &mut encoder, primitives, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(pass_label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, primitives, &screen);
        }
        let submission_start = measure_gpu_completion
            .then(|| self.gpu_completion_mailbox.reserve())
            .flatten();
        queue.submit(Some(encoder.finish()));
        if let Some(submission_start) = submission_start {
            let mailbox = Arc::clone(&self.gpu_completion_mailbox);
            queue.on_submitted_work_done(move || {
                let elapsed_nanos =
                    submission_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                mailbox.complete(elapsed_nanos);
            });
        }
        for id in &textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}

#[cfg(test)]
mod gpu_completion_tests {
    use super::{GPU_COMPLETION_SAMPLE_WINDOW, GpuCompletionMailbox, GpuCompletionTimingWindow};

    #[test]
    fn completion_window_reports_bounded_nearest_rank_p95_and_max() {
        let mut window = GpuCompletionTimingWindow::default();
        for nanos in 1..=100_u64 {
            window.push(nanos * 1_000_000);
        }

        let timing = window.snapshot();
        assert_eq!(timing.samples, 100);
        assert_eq!(timing.p95_ms, 95.0);
        assert_eq!(timing.max_ms, 100.0);
    }

    #[test]
    fn completion_window_wraps_without_exceeding_fixed_capacity() {
        let mut window = GpuCompletionTimingWindow::default();
        for nanos in 1..=(GPU_COMPLETION_SAMPLE_WINDOW as u64 + 5) {
            window.push(nanos * 1_000_000);
        }

        let timing = window.snapshot();
        assert_eq!(timing.samples, GPU_COMPLETION_SAMPLE_WINDOW);
        assert_eq!(timing.max_ms, (GPU_COMPLETION_SAMPLE_WINDOW + 5) as f32);
        assert_eq!(timing.p95_ms, (GPU_COMPLETION_SAMPLE_WINDOW - 1) as f32);
    }

    #[test]
    fn completion_mailbox_allows_one_pending_sample_and_drains_once() {
        let mailbox = GpuCompletionMailbox::default();
        assert!(mailbox.reserve().is_some());
        assert!(mailbox.reserve().is_none());

        mailbox.complete(42);
        assert_eq!(mailbox.drain_completed(), Some(42));
        assert_eq!(mailbox.drain_completed(), None);
        assert!(mailbox.reserve().is_some());
    }
}

const SHADER: &str = include_str!("splash.wgsl");
const TAU: f32 = std::f32::consts::TAU;
const PANEL_ARC: f32 = TAU * (5.0 / 12.0);

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Vertex {
    position: [f32; 3],
    uv: [f32; 2],
}

impl Vertex {
    const ATTRIBUTES: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

/// CPU mesh for the two halves of the open splash cylinder.
#[derive(Debug)]
pub struct SplashMesh {
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    first_half_index_count: u32,
}

impl SplashMesh {
    /// Two 150-degree cylinder panels separated by equal 30-degree openings.
    pub fn open_cylinder(segments_per_half: u32) -> Self {
        assert!(segments_per_half >= 2);
        let radius = 2.15;
        let half_height = 1.1;
        let mut vertices = Vec::with_capacity(((segments_per_half + 1) * 4) as usize);
        let mut indices = Vec::with_capacity((segments_per_half * 12) as usize);

        for half in 0..2 {
            let base = vertices.len() as u32;
            let panel_center = -TAU * 0.25 + half as f32 * TAU * 0.5;
            let panel_start = panel_center - PANEL_ARC * 0.5;
            for segment in 0..=segments_per_half {
                let t = segment as f32 / segments_per_half as f32;
                let angle = panel_start + t * PANEL_ARC;
                let (sin, cos) = angle.sin_cos();
                // UVs deliberately remain identical for both faces. Looking through
                // the back of this wall naturally produces the required mirror image.
                vertices.push(Vertex {
                    position: [radius * sin, -half_height, radius * cos],
                    uv: [t, 1.0],
                });
                vertices.push(Vertex {
                    position: [radius * sin, half_height, radius * cos],
                    uv: [t, 0.0],
                });
            }
            for segment in 0..segments_per_half {
                let a = base + segment * 2;
                let b = a + 1;
                let c = a + 2;
                let d = a + 3;
                // Counter-clockwise from the outer surface; culling is disabled so
                // the same geometry is visible from the interior.
                indices.extend_from_slice(&[a, c, b, b, c, d]);
            }
        }
        let first_half_index_count = segments_per_half * 6;
        Self {
            vertices,
            indices,
            first_half_index_count,
        }
    }

    #[cfg(test)]
    fn vertices(&self) -> &[Vertex] {
        &self.vertices
    }
    #[cfg(test)]
    fn indices(&self) -> &[u32] {
        &self.indices
    }
    #[cfg(test)]
    fn first_half_index_count(&self) -> u32 {
        self.first_half_index_count
    }
}

#[derive(Debug)]
pub enum RendererError {
    Image(image::ImageError),
}

impl fmt::Display for RendererError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Image(error) => write!(f, "invalid embedded splash PNG: {error}"),
        }
    }
}

impl std::error::Error for RendererError {}
impl From<image::ImageError> for RendererError {
    fn from(value: image::ImageError) -> Self {
        Self::Image(value)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    transform: [[f32; 4]; 4],
}

struct GpuTexture {
    bind_group: wgpu::BindGroup,
}

/// Borrowed decoded splash pixels. The app can retain the same RGBA allocation for its later
/// Project Hub backdrop instead of decoding the embedded PNG twice during startup.
#[derive(Clone, Copy)]
pub struct SplashRgba<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

/// One-device/one-queue renderer for the splash only.
pub struct SplashRenderer {
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    first_half_index_count: u32,
    texture_bind_groups: [GpuTexture; 2],
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    depth_view: wgpu::TextureView,
    size: wgpu::Extent3d,
}

impl SplashRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        english_png: &[u8],
        japanese_png: &[u8],
    ) -> Result<Self, RendererError> {
        let english =
            image::load_from_memory_with_format(english_png, ImageFormat::Png)?.to_rgba8();
        let japanese =
            image::load_from_memory_with_format(japanese_png, ImageFormat::Png)?.to_rgba8();
        Ok(Self::new_rgba(
            device,
            queue,
            format,
            width,
            height,
            SplashRgba {
                width: english.width(),
                height: english.height(),
                pixels: english.as_raw(),
            },
            SplashRgba {
                width: japanese.width(),
                height: japanese.height(),
                pixels: japanese.as_raw(),
            },
        ))
    }

    pub fn new_rgba(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        english: SplashRgba<'_>,
        japanese: SplashRgba<'_>,
    ) -> Self {
        let mesh = SplashMesh::open_cylinder(80);
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("splash cylinder vertices"),
            contents: bytemuck::cast_slice(&mesh.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("splash cylinder indices"),
            contents: bytemuck::cast_slice(&mesh.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("splash texture layout"),
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
        });
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("splash camera layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("splash camera"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("splash camera bind group"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });
        let texture_bind_groups = [
            load_texture_rgba(device, queue, &texture_layout, english, "English splash"),
            load_texture_rgba(device, queue, &texture_layout, japanese, "Japanese splash"),
        ];
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("splash shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("splash pipeline layout"),
            bind_group_layouts: &[Some(&camera_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("splash pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Vertex::layout()],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let size = wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        };
        let depth_view = create_depth_view(device, size);
        Self {
            pipeline,
            vertex_buffer,
            index_buffer,
            first_half_index_count: mesh.first_half_index_count,
            texture_bind_groups,
            camera_buffer,
            camera_bind_group,
            depth_view,
            size,
        }
    }

    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        self.size = wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        };
        self.depth_view = create_depth_view(device, self.size);
    }

    pub fn render(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        elapsed_seconds: f32,
    ) {
        let aspect = self.size.width as f32 / self.size.height as f32;
        let projection = directx::perspective(45_f32.to_radians(), aspect, 0.1, 100.0);
        let view_matrix = view::look_at_mat4(Vec3::new(0.0, 0.0, 7.0), Vec3::ZERO, Vec3::Y);
        let model = Mat4::from_rotation_z(-10_f32.to_radians())
            * Mat4::from_rotation_x(12_f32.to_radians())
            * Mat4::from_rotation_y(elapsed_seconds * 0.22);
        queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::bytes_of(&CameraUniform {
                transform: (projection * view_matrix * model).to_cols_array_2d(),
            }),
        );
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("splash encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("splash pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_bind_group(0, &self.camera_bind_group, &[]);
            pass.set_bind_group(1, &self.texture_bind_groups[0].bind_group, &[]);
            pass.draw_indexed(0..self.first_half_index_count, 0, 0..1);
            pass.set_bind_group(1, &self.texture_bind_groups[1].bind_group, &[]);
            pass.draw_indexed(
                self.first_half_index_count..self.first_half_index_count * 2,
                0,
                0..1,
            );
        }
        queue.submit(Some(encoder.finish()));
    }
}

fn create_depth_view(device: &wgpu::Device, size: wgpu::Extent3d) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("splash depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

fn load_texture_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    image: SplashRgba<'_>,
    label: &str,
) -> GpuTexture {
    let size = wgpu::Extent3d {
        width: image.width,
        height: image.height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        image.pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * image.width),
            rows_per_image: Some(image.height),
        },
        size,
    );
    let view = texture.create_view(&Default::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("splash sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    GpuTexture { bind_group }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cylinder_mesh_has_valid_indices_and_unit_uvs() {
        let mesh = SplashMesh::open_cylinder(8);
        assert_eq!(mesh.vertices().len(), 36);
        assert_eq!(mesh.indices().len(), 96);
        assert_eq!(mesh.first_half_index_count(), 48);
        assert!(
            mesh.indices()
                .iter()
                .all(|&index| (index as usize) < mesh.vertices().len())
        );
        assert!(mesh.vertices().iter().all(|vertex| vertex.uv[0] >= 0.0
            && vertex.uv[0] <= 1.0
            && vertex.uv[1] >= 0.0
            && vertex.uv[1] <= 1.0));
    }

    #[test]
    fn inner_surface_uses_same_uvs_as_outer_surface() {
        // Rasterization reverses the apparent horizontal direction when a wall
        // is viewed from its back. Keeping these UVs unchanged is the mirror.
        let mesh = SplashMesh::open_cylinder(4);
        for pair in mesh.vertices().chunks_exact(2) {
            assert_eq!(pair[0].uv[0], pair[1].uv[0]);
            assert_eq!(pair[0].uv[1], 1.0);
            assert_eq!(pair[1].uv[1], 0.0);
        }
    }

    #[test]
    fn cylinder_panels_have_equal_openings() {
        let mesh = SplashMesh::open_cylinder(4);
        let angle_at = |vertex_index: usize| {
            let [x, _, z] = mesh.vertices()[vertex_index].position;
            x.atan2(z)
        };
        let vertices_per_panel = (4 + 1) * 2;
        let first_start = angle_at(0);
        let first_end = angle_at(vertices_per_panel - 2);
        let second_start = angle_at(vertices_per_panel);
        let second_end = angle_at(vertices_per_panel * 2 - 2);
        let middle_gap = second_start - first_end;
        let wrap_gap = TAU - (second_end - first_start);

        assert!((middle_gap - TAU / 12.0).abs() < 1e-5);
        assert!((wrap_gap - middle_gap).abs() < 1e-5);
    }
}
