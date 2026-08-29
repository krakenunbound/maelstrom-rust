//! Retained, instanced solid-rectangle drawing for native editor surfaces.
//!
//! Coordinates are physical pixels in the render target.  A single draw call
//! emits one shader-generated unit quad for every retained [`RectInstance`].

use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};

/// Initial number of rectangles reserved on both CPU and GPU.
pub const INITIAL_RECT_CAPACITY: usize = 64 * 1024;

/// One axis-aligned, solid RGBA rectangle in physical target pixels.
///
/// `rect` is `[x, y, width, height]`; `color` is straight-alpha RGBA, each
/// component in the inclusive `0.0..=1.0` range.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct RectInstance {
    pub rect: [f32; 4],
    pub color: [f32; 4],
}

impl RectInstance {
    pub const fn new(rect: [f32; 4], color: [f32; 4]) -> Self {
        Self { rect, color }
    }
}

/// A viewport in physical render-target pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PixelViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PixelViewport {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A scissor rectangle in physical render-target pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScissorRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Physical render-target dimensions and optional output clipping for a draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectRenderRegion {
    pub target_width: u32,
    pub target_height: u32,
    pub viewport: PixelViewport,
    pub scissor: Option<ScissorRect>,
}

impl RectRenderRegion {
    pub const fn new(
        target_width: u32,
        target_height: u32,
        viewport: PixelViewport,
        scissor: Option<ScissorRect>,
    ) -> Self {
        Self {
            target_width,
            target_height,
            viewport,
            scissor,
        }
    }
}

impl ScissorRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TargetSizeUniform {
    width: f32,
    height: f32,
    _padding: [f32; 2],
}

const RECT_SHADER: &str = include_str!("rect_renderer.wgsl");

/// Reusable retained instance renderer for editor regions such as a timeline.
///
/// The CPU vector is retained and reused. The GPU buffer starts at 64k
/// rectangles and doubles only when the supplied instance count exceeds the
/// existing capacity.
pub struct TimelineRectRenderer {
    pipeline: wgpu::RenderPipeline,
    target_size_buffer: wgpu::Buffer,
    target_size_bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instances: Vec<RectInstance>,
    gpu_capacity: usize,
}

impl TimelineRectRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let target_size_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timeline rectangle target size"),
            size: std::mem::size_of::<TargetSizeUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let target_size_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("timeline rectangle target size layout"),
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
        let target_size_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("timeline rectangle target size bind group"),
            layout: &target_size_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: target_size_buffer.as_entire_binding(),
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("timeline rectangle shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("timeline rectangle pipeline layout"),
            bind_group_layouts: &[Some(&target_size_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("timeline rectangle pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[instance_layout()],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let gpu_capacity = INITIAL_RECT_CAPACITY;
        Self {
            pipeline,
            target_size_buffer,
            target_size_bind_group,
            instance_buffer: create_instance_buffer(device, gpu_capacity),
            instances: Vec::with_capacity(gpu_capacity),
            gpu_capacity,
        }
    }

    /// Replaces retained CPU instances without shrinking its allocation.
    pub fn set_instances(&mut self, instances: &[RectInstance]) {
        self.instances.clear();
        self.instances.extend_from_slice(instances);
    }

    /// Gives callers direct retained access for their own frame assembly.
    pub fn instances_mut(&mut self) -> &mut Vec<RectInstance> {
        &mut self.instances
    }

    pub fn instances(&self) -> &[RectInstance] {
        &self.instances
    }

    pub fn gpu_capacity(&self) -> usize {
        self.gpu_capacity
    }

    /// Uploads retained data. Call [`Self::paint`] later in a render pass.
    ///
    /// Dimensions are physical view pixels, not logical UI points. Empty input
    /// records no upload; [`Self::paint`] consequently records no draw.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_width: u32,
        target_height: u32,
    ) {
        if self.instances.is_empty() || target_width == 0 || target_height == 0 {
            return;
        }
        self.ensure_gpu_capacity(device, self.instances.len());
        queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&self.instances),
        );
        queue.write_buffer(
            &self.target_size_buffer,
            0,
            bytemuck::bytes_of(&TargetSizeUniform {
                width: target_width as f32,
                height: target_height as f32,
                _padding: [0.0; 2],
            }),
        );
    }

    /// Records exactly one instanced unit-quad draw for prepared instances.
    pub fn paint(&self, pass: &mut wgpu::RenderPass<'_>, region: RectRenderRegion) {
        if self.instances.is_empty()
            || region.target_width == 0
            || region.target_height == 0
            || region.viewport.width <= 0.0
            || region.viewport.height <= 0.0
        {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.target_size_bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.set_viewport(
            region.viewport.x,
            region.viewport.y,
            region.viewport.width,
            region.viewport.height,
            0.0,
            1.0,
        );
        if let Some(scissor) = region.scissor {
            pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
        }
        pass.draw(0..6, 0..self.instances.len() as u32);
    }

    fn ensure_gpu_capacity(&mut self, device: &wgpu::Device, required: usize) {
        let capacity = next_rect_capacity(self.gpu_capacity, required);
        if capacity != self.gpu_capacity {
            self.instance_buffer = create_instance_buffer(device, capacity);
            self.gpu_capacity = capacity;
        }
    }
}

/// Shared logical-point input for the native timeline rectangle callback.
///
/// Populate this before each egui frame, then install its callback early in
/// painter order so ordinary egui controls appear over the native geometry.
#[derive(Clone, Default)]
pub struct TimelineRectCallbackHandle {
    instances: Arc<Mutex<Vec<RectInstance>>>,
}

impl TimelineRectCallbackHandle {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(Mutex::new(Vec::with_capacity(INITIAL_RECT_CAPACITY))),
        }
    }

    /// Clears all logical-point rectangles from the prior egui frame.
    pub fn clear(&self) {
        self.instances
            .lock()
            .expect("timeline rectangle lock")
            .clear();
    }

    /// Appends one rectangle in egui logical points.
    pub fn push(&self, instance: RectInstance) {
        self.instances
            .lock()
            .expect("timeline rectangle lock")
            .push(instance);
    }

    /// Replaces the logical-point frame in one lock while retaining capacity.
    /// This is the preferred path for a UI-side scratch list; `push` remains
    /// useful for small direct callers and tests.
    pub fn set_instances(&self, instances: &[RectInstance]) {
        let mut retained = self.instances.lock().expect("timeline rectangle lock");
        retained.clear();
        retained.extend_from_slice(instances);
    }

    /// Adds the custom paint callback at the current painter position.
    ///
    /// Call this before regular widgets in the desired region to keep the
    /// native rectangles behind their egui labels and interaction chrome.
    pub fn install(&self, painter: &egui::Painter, rect: egui::Rect) {
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            TimelineRectCallback {
                handle: self.clone(),
            },
        ));
    }

    #[cfg(test)]
    fn snapshot(&self) -> Vec<RectInstance> {
        self.instances
            .lock()
            .expect("timeline rectangle lock")
            .clone()
    }
}

#[derive(Clone)]
struct TimelineRectCallback {
    handle: TimelineRectCallbackHandle,
}

impl egui_wgpu::CallbackTrait for TimelineRectCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(renderer) = resources.get_mut::<TimelineRectRenderer>() else {
            return Vec::new();
        };
        let input = self
            .handle
            .instances
            .lock()
            .expect("timeline rectangle lock");
        renderer.instances.clear();
        renderer.instances.extend(
            input
                .iter()
                .copied()
                .map(|rect| rect_points_to_pixels(rect, screen.pixels_per_point)),
        );
        renderer.prepare(
            device,
            queue,
            screen.size_in_pixels[0],
            screen.size_in_pixels[1],
        );
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(renderer) = resources.get::<TimelineRectRenderer>() else {
            return;
        };
        let clip = info.clip_rect_in_pixels();
        if clip.width_px <= 0 || clip.height_px <= 0 {
            return;
        }
        // egui sets the callback viewport first. The timeline intentionally
        // uses target-wide pixel positions, but retains egui's clip rectangle.
        renderer.paint(
            pass,
            RectRenderRegion::new(
                info.screen_size_px[0],
                info.screen_size_px[1],
                PixelViewport::new(
                    0.0,
                    0.0,
                    info.screen_size_px[0] as f32,
                    info.screen_size_px[1] as f32,
                ),
                Some(ScissorRect::new(
                    clip.left_px as u32,
                    clip.top_px as u32,
                    clip.width_px as u32,
                    clip.height_px as u32,
                )),
            ),
        );
    }
}

fn rect_points_to_pixels(mut rect: RectInstance, pixels_per_point: f32) -> RectInstance {
    rect.rect = rect.rect.map(|component| component * pixels_per_point);
    rect
}

fn instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRIBUTES: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<RectInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

fn create_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timeline rectangle instances"),
        size: (capacity * std::mem::size_of::<RectInstance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Returns the smallest doubling capacity which holds `required` instances.
/// It returns `current` unchanged when sufficient, so ordinary frames never
/// recreate GPU buffers.
pub const fn next_rect_capacity(current: usize, required: usize) -> usize {
    let mut capacity = if current < INITIAL_RECT_CAPACITY {
        INITIAL_RECT_CAPACITY
    } else {
        current
    };
    while capacity < required {
        if capacity > usize::MAX / 2 {
            return required;
        }
        capacity *= 2;
    }
    capacity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_timeline_shader_is_valid_current_wgsl() {
        naga::front::wgsl::parse_str(RECT_SHADER).expect("solid timeline WGSL");
    }

    #[test]
    #[ignore = "requires a real GPU adapter; run in release with --ignored --nocapture"]
    fn gpu_executes_banded_timeline_frames_under_eight_ms_p95() {
        use std::time::{Duration, Instant};

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("foundation GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("timeline GPU evidence device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("foundation GPU device");
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("timeline GPU evidence target"),
            size: wgpu::Extent3d {
                width: 1_920,
                height: 1_080,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut renderer = TimelineRectRenderer::new(&device, format);
        let _texture_renderer =
            crate::texture_renderer::TimelineTextureRenderer::new(&device, format);
        let instances = (0..1_000)
            .map(|index| {
                let x = (index % 100) as f32 * 19.2;
                let y = (index / 100) as f32 * 82.0 + 120.0;
                RectInstance::new([x, y, 18.0, 70.0], [0.08, 0.38, 0.56, 1.0])
            })
            .collect::<Vec<_>>();
        renderer.set_instances(&instances);

        let mut samples = Vec::with_capacity(240);
        for frame in 0..260 {
            let started = Instant::now();
            renderer.prepare(&device, &queue, 1_920, 1_080);
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("timeline GPU evidence encoder"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("timeline GPU evidence pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
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
                renderer.paint(
                    &mut pass,
                    RectRenderRegion::new(
                        1_920,
                        1_080,
                        PixelViewport::new(0.0, 0.0, 1_920.0, 1_080.0),
                        None,
                    ),
                );
            }
            let submission = queue.submit([encoder.finish()]);
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(Duration::from_secs(1)),
                })
                .expect("timeline GPU completion");
            if frame >= 20 {
                samples.push(started.elapsed());
            }
        }
        samples.sort_unstable();
        let p95 = samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)];
        eprintln!(
            "timeline GPU evidence: adapter={} p95={p95:?} instances={}",
            adapter.get_info().name,
            instances.len()
        );
        assert!(p95 < Duration::from_millis(8), "GPU p95 was {p95:?}");
    }

    #[test]
    fn rect_instance_is_tightly_packed_for_the_gpu() {
        assert_eq!(std::mem::size_of::<RectInstance>(), 32);
        assert_eq!(std::mem::align_of::<RectInstance>(), 4);
        let instance = RectInstance::new([1.0, 2.0, 3.0, 4.0], [0.1, 0.2, 0.3, 0.4]);
        let packed: &[f32] = bytemuck::cast_slice(std::slice::from_ref(&instance));
        assert_eq!(packed, &[1.0, 2.0, 3.0, 4.0, 0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn capacity_starts_at_64k_and_only_grows_when_needed() {
        assert_eq!(next_rect_capacity(0, 1), INITIAL_RECT_CAPACITY);
        assert_eq!(
            next_rect_capacity(INITIAL_RECT_CAPACITY, INITIAL_RECT_CAPACITY),
            INITIAL_RECT_CAPACITY
        );
        assert_eq!(
            next_rect_capacity(INITIAL_RECT_CAPACITY, INITIAL_RECT_CAPACITY + 1),
            INITIAL_RECT_CAPACITY * 2
        );
        assert_eq!(
            next_rect_capacity(INITIAL_RECT_CAPACITY * 2, INITIAL_RECT_CAPACITY + 1),
            INITIAL_RECT_CAPACITY * 2
        );
    }

    #[test]
    fn logical_points_convert_to_physical_pixels() {
        let pixels = rect_points_to_pixels(
            RectInstance::new([2.0, 3.5, 10.0, 4.0], [0.1, 0.2, 0.3, 0.4]),
            1.5,
        );
        assert_eq!(pixels.rect, [3.0, 5.25, 15.0, 6.0]);
        assert_eq!(pixels.color, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn callback_handle_clears_prior_frame_instances() {
        let handle = TimelineRectCallbackHandle::new();
        handle.push(RectInstance::default());
        assert_eq!(handle.snapshot().len(), 1);
        handle.clear();
        assert!(handle.snapshot().is_empty());
    }
}
