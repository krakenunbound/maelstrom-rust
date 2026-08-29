//! Retained, instanced textured-rectangle drawing for native timeline surfaces.
//!
//! The renderer accepts logical egui point coordinates through its callback
//! handle, converts them once in `prepare`, then draws contiguous runs sharing
//! a texture in submission order. This keeps clip-thumbnail strips native
//! without changing their painter order relative to solid timeline geometry.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use bytemuck::{Pod, Zeroable};

use crate::rect_renderer::{PixelViewport, RectRenderRegion, ScissorRect};

/// Initial number of textured rectangles reserved on both CPU and GPU.
pub const INITIAL_TEXTURE_INSTANCE_CAPACITY: usize = 16 * 1024;

/// One textured rectangle in physical target pixels.
///
/// `rect` is `[x, y, width, height]`; `uv` is `[min_x, min_y, max_x, max_y]`;
/// and `tint` is straight-alpha RGBA in the inclusive `0.0..=1.0` range.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TextureInstance {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub tint: [f32; 4],
}

impl TextureInstance {
    pub const fn new(rect: [f32; 4], uv: [f32; 4], tint: [f32; 4]) -> Self {
        Self { rect, uv, tint }
    }
}

/// A textured rectangle associated with one caller-owned native texture key.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TexturedRect {
    pub texture_id: u64,
    pub instance: TextureInstance,
}

impl TexturedRect {
    pub const fn new(texture_id: u64, instance: TextureInstance) -> Self {
        Self {
            texture_id,
            instance,
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

const TEXTURE_SHADER: &str = include_str!("texture_renderer.wgsl");

struct GpuTexture {
    // Keep the allocation alive for as long as the bind group is registered.
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Validation error returned when a RGBA upload cannot describe a 2D image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextureUploadError {
    ZeroDimension,
    InvalidRgbaLength { expected: usize, actual: usize },
}

impl fmt::Display for TextureUploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension => write!(f, "texture dimensions must both be non-zero"),
            Self::InvalidRgbaLength { expected, actual } => write!(
                f,
                "RGBA byte length must be {expected} for these dimensions, got {actual}"
            ),
        }
    }
}

impl std::error::Error for TextureUploadError {}

/// Reusable retained texture-strip renderer for timeline thumbnails.
pub struct TimelineTextureRenderer {
    pipeline: wgpu::RenderPipeline,
    target_size_buffer: wgpu::Buffer,
    target_size_bind_group: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    instance_buffer: wgpu::Buffer,
    instances: Vec<TexturedRect>,
    packed_instances: Vec<TextureInstance>,
    gpu_capacity: usize,
    textures: HashMap<u64, GpuTexture>,
}

impl TimelineTextureRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let target_size_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timeline texture target size"),
            size: std::mem::size_of::<TargetSizeUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let target_size_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("timeline texture target size layout"),
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
            label: Some("timeline texture target size bind group"),
            layout: &target_size_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: target_size_buffer.as_entire_binding(),
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("timeline texture image layout"),
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("timeline texture sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("timeline texture shader"),
            source: wgpu::ShaderSource::Wgsl(TEXTURE_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("timeline texture pipeline layout"),
            bind_group_layouts: &[Some(&target_size_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("timeline texture pipeline"),
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
        let gpu_capacity = INITIAL_TEXTURE_INSTANCE_CAPACITY;
        Self {
            pipeline,
            target_size_buffer,
            target_size_bind_group,
            texture_layout,
            sampler,
            instance_buffer: create_instance_buffer(device, gpu_capacity),
            instances: Vec::with_capacity(gpu_capacity),
            packed_instances: Vec::with_capacity(gpu_capacity),
            gpu_capacity,
            textures: HashMap::new(),
        }
    }

    /// Replaces retained CPU instances without shrinking their allocation.
    pub fn set_instances(&mut self, instances: &[TexturedRect]) {
        self.instances.clear();
        self.instances.extend_from_slice(instances);
    }

    pub fn instances(&self) -> &[TexturedRect] {
        &self.instances
    }

    pub fn gpu_capacity(&self) -> usize {
        self.gpu_capacity
    }

    /// Uploads one tightly packed sRGBA texture, replacing an existing key.
    pub fn upload_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture_id: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), TextureUploadError> {
        validate_rgba_length(width, height, rgba.len())?;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("timeline texture image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
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
            size,
        );
        let view = texture.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("timeline texture image bind group"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.textures.insert(
            texture_id,
            GpuTexture {
                _texture: texture,
                bind_group,
            },
        );
        Ok(())
    }

    pub fn remove_texture(&mut self, texture_id: u64) -> bool {
        self.textures.remove(&texture_id).is_some()
    }

    pub fn clear_textures(&mut self) {
        self.textures.clear();
    }

    /// Uploads retained instances and target dimensions once per egui frame.
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
        self.packed_instances.clear();
        self.packed_instances
            .extend(self.instances.iter().map(|item| item.instance));
        queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&self.packed_instances),
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

    /// Draws one call for each contiguous submitted texture-key run.
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
        let mut start = 0_usize;
        while start < self.instances.len() {
            let texture_id = self.instances[start].texture_id;
            let mut end = start + 1;
            while end < self.instances.len() && self.instances[end].texture_id == texture_id {
                end += 1;
            }
            let Some(texture) = self.textures.get(&texture_id) else {
                start = end;
                continue;
            };
            pass.set_bind_group(1, &texture.bind_group, &[]);
            pass.draw(0..6, start as u32..end as u32);
            start = end;
        }
    }

    fn ensure_gpu_capacity(&mut self, device: &wgpu::Device, required: usize) {
        let capacity = next_texture_capacity(self.gpu_capacity, required);
        if capacity != self.gpu_capacity {
            self.instance_buffer = create_instance_buffer(device, capacity);
            self.gpu_capacity = capacity;
        }
    }
}

/// A contiguous texture-key range within submitted instance order.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureBatch {
    pub texture_id: u64,
    pub instances: std::ops::Range<u32>,
}

/// Produces the minimum draw batches that preserve submitted texture ordering.
#[cfg(test)]
pub fn contiguous_texture_batches(instances: &[TexturedRect]) -> Vec<TextureBatch> {
    let mut batches = Vec::new();
    let Some(first) = instances.first() else {
        return batches;
    };
    let mut texture_id = first.texture_id;
    let mut start = 0_u32;
    for (index, item) in instances.iter().enumerate().skip(1) {
        if item.texture_id != texture_id {
            batches.push(TextureBatch {
                texture_id,
                instances: start..index as u32,
            });
            texture_id = item.texture_id;
            start = index as u32;
        }
    }
    batches.push(TextureBatch {
        texture_id,
        instances: start..instances.len() as u32,
    });
    batches
}

/// Shared logical-point input for the native timeline texture callback.
#[derive(Clone, Default)]
pub struct TimelineTextureCallbackHandle {
    instances: Arc<Mutex<Vec<TexturedRect>>>,
}

impl TimelineTextureCallbackHandle {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(Mutex::new(Vec::with_capacity(
                INITIAL_TEXTURE_INSTANCE_CAPACITY,
            ))),
        }
    }

    pub fn clear(&self) {
        self.instances
            .lock()
            .expect("timeline texture lock")
            .clear();
    }

    /// Replaces one logical-point frame with a single lock acquisition.
    pub fn set_instances(&self, instances: &[TexturedRect]) {
        let mut retained = self.instances.lock().expect("timeline texture lock");
        retained.clear();
        retained.extend_from_slice(instances);
    }

    /// Adds the texture callback at the current painter position.
    ///
    /// Install this after the solid rectangle callback when textured clips must
    /// appear over their background fills.
    pub fn install(&self, painter: &egui::Painter, rect: egui::Rect) {
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            TimelineTextureCallback {
                handle: self.clone(),
            },
        ));
    }

    #[cfg(test)]
    fn snapshot(&self) -> Vec<TexturedRect> {
        self.instances
            .lock()
            .expect("timeline texture lock")
            .clone()
    }
}

#[derive(Clone)]
struct TimelineTextureCallback {
    handle: TimelineTextureCallbackHandle,
}

impl egui_wgpu::CallbackTrait for TimelineTextureCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(renderer) = resources.get_mut::<TimelineTextureRenderer>() else {
            return Vec::new();
        };
        let input = self.handle.instances.lock().expect("timeline texture lock");
        renderer.instances.clear();
        renderer.instances.extend(input.iter().copied().map(|item| {
            TexturedRect::new(
                item.texture_id,
                texture_points_to_pixels(item.instance, screen.pixels_per_point),
            )
        }));
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
        let Some(renderer) = resources.get::<TimelineTextureRenderer>() else {
            return;
        };
        let clip = info.clip_rect_in_pixels();
        if clip.width_px <= 0 || clip.height_px <= 0 {
            return;
        }
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

fn texture_points_to_pixels(
    mut instance: TextureInstance,
    pixels_per_point: f32,
) -> TextureInstance {
    instance.rect = instance.rect.map(|component| component * pixels_per_point);
    instance
}

fn rgba_byte_len(width: u32, height: u32) -> Result<usize, TextureUploadError> {
    if width == 0 || height == 0 {
        return Err(TextureUploadError::ZeroDimension);
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .and_then(|count| count.checked_mul(4))
        .ok_or(TextureUploadError::ZeroDimension)?;
    Ok(pixels)
}

fn validate_rgba_length(width: u32, height: u32, actual: usize) -> Result<(), TextureUploadError> {
    let expected = rgba_byte_len(width, height)?;
    if actual != expected {
        return Err(TextureUploadError::InvalidRgbaLength { expected, actual });
    }
    Ok(())
}

fn instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRIBUTES: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<TextureInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

fn create_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timeline texture instances"),
        size: (capacity * std::mem::size_of::<TextureInstance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Returns the smallest doubling capacity which holds `required` instances.
pub const fn next_texture_capacity(current: usize, required: usize) -> usize {
    let mut capacity = if current < INITIAL_TEXTURE_INSTANCE_CAPACITY {
        INITIAL_TEXTURE_INSTANCE_CAPACITY
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
    fn textured_timeline_shader_is_valid_current_wgsl() {
        naga::front::wgsl::parse_str(TEXTURE_SHADER).expect("textured timeline WGSL");
    }

    #[test]
    fn texture_instance_is_tightly_packed_for_the_gpu() {
        assert_eq!(std::mem::size_of::<TextureInstance>(), 48);
        assert_eq!(std::mem::align_of::<TextureInstance>(), 4);
        let instance = TextureInstance::new(
            [1.0, 2.0, 3.0, 4.0],
            [0.1, 0.2, 0.3, 0.4],
            [0.5, 0.6, 0.7, 0.8],
        );
        let packed: &[f32] = bytemuck::cast_slice(std::slice::from_ref(&instance));
        assert_eq!(
            packed,
            &[1.0, 2.0, 3.0, 4.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]
        );
    }

    #[test]
    fn capacity_starts_at_16k_and_doubles_only_when_needed() {
        assert_eq!(
            next_texture_capacity(0, 1),
            INITIAL_TEXTURE_INSTANCE_CAPACITY
        );
        assert_eq!(
            next_texture_capacity(
                INITIAL_TEXTURE_INSTANCE_CAPACITY,
                INITIAL_TEXTURE_INSTANCE_CAPACITY
            ),
            INITIAL_TEXTURE_INSTANCE_CAPACITY
        );
        assert_eq!(
            next_texture_capacity(
                INITIAL_TEXTURE_INSTANCE_CAPACITY,
                INITIAL_TEXTURE_INSTANCE_CAPACITY + 1
            ),
            INITIAL_TEXTURE_INSTANCE_CAPACITY * 2
        );
    }

    #[test]
    fn logical_points_preserve_uv_tint_and_texture_key() {
        let source = TexturedRect::new(
            55,
            TextureInstance::new(
                [2.0, 3.5, 10.0, 4.0],
                [0.1, 0.2, 0.9, 0.8],
                [0.3, 0.4, 0.5, 0.6],
            ),
        );
        let pixels = TexturedRect::new(
            source.texture_id,
            texture_points_to_pixels(source.instance, 1.5),
        );
        assert_eq!(pixels.texture_id, 55);
        assert_eq!(pixels.instance.rect, [3.0, 5.25, 15.0, 6.0]);
        assert_eq!(pixels.instance.uv, source.instance.uv);
        assert_eq!(pixels.instance.tint, source.instance.tint);
    }

    #[test]
    fn rejects_invalid_rgba_lengths_without_a_device() {
        assert_eq!(rgba_byte_len(0, 4), Err(TextureUploadError::ZeroDimension));
        assert_eq!(rgba_byte_len(2, 3), Ok(24));
        assert_eq!(
            validate_rgba_length(2, 3, 23),
            Err(TextureUploadError::InvalidRgbaLength {
                expected: 24,
                actual: 23,
            })
        );
    }

    #[test]
    fn batches_only_contiguous_texture_runs() {
        let item = |texture_id| TexturedRect::new(texture_id, TextureInstance::default());
        assert_eq!(
            contiguous_texture_batches(&[item(1), item(1), item(2), item(1), item(1)]),
            vec![
                TextureBatch {
                    texture_id: 1,
                    instances: 0..2
                },
                TextureBatch {
                    texture_id: 2,
                    instances: 2..3
                },
                TextureBatch {
                    texture_id: 1,
                    instances: 3..5
                },
            ]
        );
    }

    #[test]
    fn handle_clear_and_set_replace_the_retained_frame() {
        let handle = TimelineTextureCallbackHandle::new();
        handle.set_instances(&[TexturedRect::new(4, TextureInstance::default())]);
        assert_eq!(handle.snapshot().len(), 1);
        handle.clear();
        assert!(handle.snapshot().is_empty());
    }
}
