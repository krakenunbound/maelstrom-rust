//! Native NVIDIA RTX Video Super Resolution.
//!
//! Calls NVIDIA Video Effects `VideoSuperRes` through the C API. The
//! proprietary SDK DLLs are an optional runtime under `rtx-vsr/`.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use libloading::Library;

const VERIFIED_MAX_EDGE: u32 = 15_360;
const MAX_OUTPUT_EDGE: u32 = 16_384;
const ALIGNMENT: u32 = 8;
const WIDTH_ALIGNMENT: u32 = 64;
const GPU_ROW_ALIGNMENT: u32 = 0;
const MAX_SCALE: u32 = 4;

const NVCV_RGBA: c_int = 6;
const NVCV_U8: c_int = 1;
const NVCV_CHUNKY: u32 = 0;
const NVCV_CPU: u32 = 0;
const NVCV_GPU: u32 = 1;
const FALLBACK_VSR_SCALE: u32 = 2;

const DLLS: &[&str] = &[
    "nppc64_12.dll",
    "nppial64_12.dll",
    "nppicc64_12.dll",
    "nppidei64_12.dll",
    "nppif64_12.dll",
    "nppig64_12.dll",
    "nppim64_12.dll",
    "nppist64_12.dll",
    "nppitc64_12.dll",
    "nvinfer_10.dll",
    "nvinfer_plugin_10.dll",
    "nvonnxparser_10.dll",
    "NVCVImage.dll",
    "nvngxruntime.dll",
    "NVVideoEffects.dll",
    "nvVFXVideoSuperRes.dll",
    "nvngx_vsr.dll",
];

/// Tightly packed rgb24 frame, same layout as Kraken Upscale.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Low = 1,
    Medium = 2,
    High = 3,
    Ultra = 4,
}

impl Quality {
    pub fn from_index(index: i32) -> Self {
        match index {
            0 => Self::Low,
            1 => Self::Medium,
            2 => Self::High,
            _ => Self::Ultra,
        }
    }

    pub fn from_u8(index: u8) -> Self {
        Self::from_index(index as i32)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Ultra => "Ultra",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DimensionPlan {
    pub output_width: u32,
    pub output_height: u32,
    pub vsr_width: u32,
    pub vsr_height: u32,
}

impl DimensionPlan {
    pub fn uses_hybrid(self) -> bool {
        (self.output_width, self.output_height) != (self.vsr_width, self.vsr_height)
    }
}

pub fn runtime_dir() -> Option<PathBuf> {
    let override_dir = std::env::var_os("KRAKEN_RTX_VSR_DIR").map(PathBuf::from);
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("rtx-vsr")));
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("rtx-vsr");
    let packaged_app = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("rtx-vsr");
    [
        override_dir,
        beside_exe,
        Some(workspace),
        Some(packaged_app),
    ]
    .into_iter()
    .flatten()
    .find(|dir| DLLS.iter().all(|name| dir.join(name).is_file()))
}

pub fn available() -> bool {
    runtime_dir().is_some()
}

pub fn plan_output(
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
) -> Result<DimensionPlan, String> {
    plan_output_inner(input_width, input_height, output_width, output_height, None)
}

fn plan_output_inner(
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
    force_scale: Option<u32>,
) -> Result<DimensionPlan, String> {
    if input_width == 0 || input_height == 0 || output_width == 0 || output_height == 0 {
        return Err("RTX VSR output dimensions must be positive".into());
    }
    if output_width.max(output_height) > MAX_OUTPUT_EDGE {
        return Err(format!(
            "RTX VSR supports a maximum output edge of {MAX_OUTPUT_EDGE}px; requested {output_width}×{output_height}"
        ));
    }
    if output_width < input_width || output_height < input_height {
        return Err(format!(
            "RTX VSR only upscales; {input_width}×{input_height} cannot become {output_width}×{output_height}"
        ));
    }
    let width_scale = output_width.saturating_add(input_width - 1) / input_width;
    let height_scale = output_height.saturating_add(input_height - 1) / input_height;
    if width_scale > MAX_SCALE || height_scale > MAX_SCALE {
        return Err(format!(
            "RTX VSR supports at most {MAX_SCALE}x; {input_width}×{input_height} → {output_width}×{output_height} needs more"
        ));
    }

    let padded_width = align_width(input_width);
    let padded_height = align_up(input_height);
    let mut vsr_width;
    let mut vsr_height;
    if let Some(scale) = force_scale {
        vsr_width = padded_width.saturating_mul(scale);
        vsr_height = padded_height.saturating_mul(scale);
    } else {
        let scale_x = output_width as f64 / input_width as f64;
        let scale_y = output_height as f64 / input_height as f64;
        vsr_width = align_width(((padded_width as f64 * scale_x).round() as u32).max(1));
        vsr_height = align_up(((padded_height as f64 * scale_y).round() as u32).max(1));
    }

    if vsr_width.max(vsr_height) > VERIFIED_MAX_EDGE {
        let ratio = VERIFIED_MAX_EDGE as f64 / vsr_width.max(vsr_height) as f64;
        vsr_width = align_to_down(vsr_width as f64 * ratio, WIDTH_ALIGNMENT);
        vsr_height = align_to_down(vsr_height as f64 * ratio, ALIGNMENT);
    }
    if vsr_width < padded_width || vsr_height < padded_height {
        return Err("RTX VSR protected intermediate would downscale the source".into());
    }

    Ok(DimensionPlan {
        output_width,
        output_height,
        vsr_width,
        vsr_height,
    })
}

pub fn upscale_frame(
    source: &Frame,
    target_width: u32,
    target_height: u32,
    quality: Quality,
) -> Result<Frame, String> {
    let mut session = Session::open(
        source.width,
        source.height,
        target_width,
        target_height,
        quality,
        false,
    )?;
    session.enhance(source)
}

fn shared_api() -> Result<&'static Api, String> {
    static API: Mutex<Option<&'static Api>> = Mutex::new(None);
    let mut slot = API
        .lock()
        .map_err(|_| "NVIDIA RTX VSR runtime lock was poisoned".to_string())?;
    if let Some(api) = *slot {
        return Ok(api);
    }
    let runtime = runtime_dir().ok_or_else(|| {
        "NVIDIA RTX VSR runtime missing — expected the rtx-vsr folder beside Maelstrom.exe"
            .to_string()
    })?;
    let api = Box::leak(Box::new(unsafe { Api::load(&runtime)? }));
    *slot = Some(api);
    Ok(api)
}

/// Persistent VideoSuperRes handle. Reusing one effect across frames is what
/// gives NVIDIA's temporal model its memory; creating it per frame does not.
pub struct Session {
    api: &'static Api,
    effect: NvEffect,
    input_gpu: NvCvImage,
    output_gpu: NvCvImage,
    source_width: u32,
    source_height: u32,
    src_width: u32,
    src_height: u32,
    plan: DimensionPlan,
    quality: Quality,
    temporal: bool,
    used_format_fallback: bool,
}

impl Session {
    pub fn open(
        src_width: u32,
        src_height: u32,
        target_width: u32,
        target_height: u32,
        quality: Quality,
        temporal: bool,
    ) -> Result<Self, String> {
        let plan = plan_output(src_width, src_height, target_width, target_height)?;
        let api = shared_api()?;
        let mut session = Self {
            api,
            effect: std::ptr::null_mut(),
            input_gpu: NvCvImage::default(),
            output_gpu: NvCvImage::default(),
            source_width: src_width,
            source_height: src_height,
            src_width: align_width(src_width),
            src_height: align_up(src_height),
            plan,
            quality,
            temporal,
            used_format_fallback: false,
        };
        unsafe {
            session.alloc_gpu()?;
            session.create_effect()?;
        }
        Ok(session)
    }

    pub fn enhance(&mut self, source: &Frame) -> Result<Frame, String> {
        let working = pad_to_alignment(source);
        if working.width != self.src_width || working.height != self.src_height {
            return Err(format!(
                "RTX VSR session is {}×{}; received {}×{}",
                self.src_width, self.src_height, source.width, source.height
            ));
        }
        let mut enhanced = match unsafe { self.run_frame(&working) } {
            Err(error) if !self.used_format_fallback && is_pixel_format_error(&error) => {
                self.rebuild_with_fallback_scale()?;
                unsafe { self.run_frame(&working)? }
            }
            other => other?,
        };
        if (working.width, working.height) != (source.width, source.height) {
            let scale_x = enhanced.width as f32 / working.width as f32;
            let scale_y = enhanced.height as f32 / working.height as f32;
            let crop_w = ((source.width as f32 * scale_x).round() as u32).max(1);
            let crop_h = ((source.height as f32 * scale_y).round() as u32).max(1);
            enhanced = crop_frame(
                &enhanced,
                crop_w.min(enhanced.width),
                crop_h.min(enhanced.height),
            );
        }
        assert_channel_integrity(source, &enhanced)?;
        if (enhanced.width, enhanced.height) != (self.plan.output_width, self.plan.output_height) {
            Ok(resample_bicubic(
                &enhanced,
                self.plan.output_width,
                self.plan.output_height,
            ))
        } else {
            Ok(enhanced)
        }
    }

    pub fn reset_shot(&mut self) -> Result<(), String> {
        unsafe {
            self.destroy_effect();
            self.create_effect()
        }
    }

    unsafe fn alloc_gpu(&mut self) -> Result<(), String> {
        self.api.check(
            // SAFETY: dimensions and image storage belong to this session; the loaded API
            // function is retained by `self.api` for the allocation's lifetime.
            unsafe {
                (self.api.image_alloc)(
                    &mut self.input_gpu,
                    self.src_width,
                    self.src_height,
                    NVCV_RGBA,
                    NVCV_U8,
                    NVCV_CHUNKY,
                    NVCV_GPU,
                    GPU_ROW_ALIGNMENT,
                )
            },
            "allocating RTX VSR input on the GPU",
        )?;
        self.api.check(
            // SAFETY: output dimensions and storage are session-owned and valid for the FFI call.
            unsafe {
                (self.api.image_alloc)(
                    &mut self.output_gpu,
                    self.plan.vsr_width,
                    self.plan.vsr_height,
                    NVCV_RGBA,
                    NVCV_U8,
                    NVCV_CHUNKY,
                    NVCV_GPU,
                    GPU_ROW_ALIGNMENT,
                )
            },
            "allocating RTX VSR output on the GPU",
        )
    }

    unsafe fn create_effect(&mut self) -> Result<(), String> {
        self.api.check(
            // SAFETY: the effect name is NUL-terminated and `self.effect` is writable.
            unsafe { (self.api.create_effect)(c"VideoSuperRes".as_ptr(), &mut self.effect) },
            "creating NVIDIA VideoSuperRes",
        )?;
        let result = (|| {
            self.api.check(
                // SAFETY: `self.effect` was created above and the property name is NUL-terminated.
                unsafe {
                    (self.api.set_u32)(self.effect, c"QualityLevel".as_ptr(), self.quality as u32)
                },
                "setting RTX VSR quality",
            )?;
            let _ = self.temporal;
            self.api.check(
                // SAFETY: the created effect accepts the initialized session input image.
                unsafe {
                    (self.api.set_image)(self.effect, c"SrcImage0".as_ptr(), &mut self.input_gpu)
                },
                "setting RTX VSR input",
            )?;
            self.api.check(
                // SAFETY: the created effect accepts the initialized session output image.
                unsafe {
                    (self.api.set_image)(self.effect, c"DstImage0".as_ptr(), &mut self.output_gpu)
                },
                "setting RTX VSR output",
            )?;
            self.api.check(
                // SAFETY: a null stream selects the API's default CUDA stream.
                unsafe {
                    (self.api.set_stream)(self.effect, c"CudaStream".as_ptr(), std::ptr::null_mut())
                },
                "setting RTX VSR CUDA stream",
            )?;
            self.api.check(
                // SAFETY: the effect and all required properties were initialized above.
                unsafe { (self.api.load_effect)(self.effect) },
                "loading NVIDIA VideoSuperRes",
            )
        })();
        if result.is_err() {
            // SAFETY: this function owns the effect handle and is its sole destroyer.
            unsafe { self.destroy_effect() };
        }
        result
    }

    unsafe fn run_frame(&mut self, source: &Frame) -> Result<Frame, String> {
        let mut input_rgba = pack_rgba8(source);
        let mut output_rgba =
            vec![0u8; self.plan.vsr_width as usize * self.plan.vsr_height as usize * 4];
        let mut input_cpu = NvCvImage::default();
        let mut output_cpu = NvCvImage::default();
        self.api.check(
            // SAFETY: `input_rgba` remains allocated and has one tightly packed RGBA row per source row.
            unsafe {
                (self.api.image_init)(
                    &mut input_cpu,
                    source.width,
                    source.height,
                    (source.width * 4) as c_int,
                    input_rgba.as_mut_ptr().cast(),
                    NVCV_RGBA,
                    NVCV_U8,
                    NVCV_CHUNKY,
                    NVCV_CPU,
                )
            },
            "initializing RTX VSR input",
        )?;
        self.api.check(
            // SAFETY: `output_rgba` remains allocated and has sufficient tightly packed RGBA storage.
            unsafe {
                (self.api.image_init)(
                    &mut output_cpu,
                    self.plan.vsr_width,
                    self.plan.vsr_height,
                    (self.plan.vsr_width * 4) as c_int,
                    output_rgba.as_mut_ptr().cast(),
                    NVCV_RGBA,
                    NVCV_U8,
                    NVCV_CHUNKY,
                    NVCV_CPU,
                )
            },
            "initializing RTX VSR output",
        )?;
        self.api.check(
            // SAFETY: both images are initialized for the current frame and null selects default stream/event.
            unsafe {
                (self.api.image_transfer)(
                    &input_cpu,
                    &mut self.input_gpu,
                    1.0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            "uploading RTX VSR input",
        )?;
        self.api.check(
            // SAFETY: the loaded effect references the live session GPU images.
            unsafe { (self.api.run_effect)(self.effect, 0) },
            &format!(
                "running NVIDIA VideoSuperRes {}×{} → {}×{}",
                source.width, source.height, self.plan.vsr_width, self.plan.vsr_height
            ),
        )?;
        self.api.check(
            // SAFETY: both images are initialized for the current frame and null selects default stream/event.
            unsafe {
                (self.api.image_transfer)(
                    &self.output_gpu,
                    &mut output_cpu,
                    1.0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            "downloading RTX VSR output",
        )?;
        unpack_rgba8(&output_rgba, self.plan.vsr_width, self.plan.vsr_height)
    }

    fn rebuild_with_fallback_scale(&mut self) -> Result<(), String> {
        self.used_format_fallback = true;
        self.plan = plan_output_inner(
            self.source_width,
            self.source_height,
            self.plan.output_width,
            self.plan.output_height,
            Some(FALLBACK_VSR_SCALE),
        )?;
        unsafe {
            self.destroy_effect();
            self.dealloc_gpu();
            self.alloc_gpu()?;
            self.create_effect()
        }
    }

    unsafe fn dealloc_gpu(&mut self) {
        if !self.input_gpu.delete_ptr.is_null() {
            // SAFETY: this session owns the initialized GPU image and deallocates it once.
            unsafe { (self.api.image_dealloc)(&mut self.input_gpu) };
            self.input_gpu = NvCvImage::default();
        }
        if !self.output_gpu.delete_ptr.is_null() {
            // SAFETY: this session owns the initialized GPU image and deallocates it once.
            unsafe { (self.api.image_dealloc)(&mut self.output_gpu) };
            self.output_gpu = NvCvImage::default();
        }
    }

    unsafe fn destroy_effect(&mut self) {
        if !self.effect.is_null() {
            // SAFETY: this session owns the live effect handle and destroys it once.
            unsafe { (self.api.destroy_effect)(self.effect) };
            self.effect = std::ptr::null_mut();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            self.destroy_effect();
            self.dealloc_gpu();
        }
    }
}

fn align_to_down(value: f64, alignment: u32) -> u32 {
    ((value.floor() as u32) / alignment * alignment).max(alignment)
}

fn is_pixel_format_error(error: &str) -> bool {
    error.contains("code -9") || error.contains("pixel format")
}

fn align_up(value: u32) -> u32 {
    value.saturating_add(ALIGNMENT - 1) / ALIGNMENT * ALIGNMENT
}

fn align_width(value: u32) -> u32 {
    value.saturating_add(WIDTH_ALIGNMENT - 1) / WIDTH_ALIGNMENT * WIDTH_ALIGNMENT
}

fn pad_to_alignment(source: &Frame) -> Frame {
    let width = align_width(source.width);
    let height = align_up(source.height);
    if width == source.width && height == source.height {
        return Frame {
            width,
            height,
            rgb: source.rgb.clone(),
        };
    }
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    for y in 0..height as usize {
        let src_y = y.min(source.height as usize - 1);
        for x in 0..width as usize {
            let src_x = x.min(source.width as usize - 1);
            let src = (src_y * source.width as usize + src_x) * 3;
            let dst = (y * width as usize + x) * 3;
            rgb[dst..dst + 3].copy_from_slice(&source.rgb[src..src + 3]);
        }
    }
    Frame { width, height, rgb }
}

fn crop_frame(source: &Frame, width: u32, height: u32) -> Frame {
    if source.width == width && source.height == height {
        return Frame {
            width,
            height,
            rgb: source.rgb.clone(),
        };
    }
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    let copy_w = width.min(source.width) as usize;
    let copy_h = height.min(source.height) as usize;
    for y in 0..copy_h {
        let src = y * source.width as usize * 3;
        let dst = y * width as usize * 3;
        rgb[dst..dst + copy_w * 3].copy_from_slice(&source.rgb[src..src + copy_w * 3]);
    }
    Frame { width, height, rgb }
}

fn assert_channel_integrity(source: &Frame, output: &Frame) -> Result<(), String> {
    let means = |frame: &Frame| {
        let step = (frame.width.max(frame.height) / 512).max(1) as usize;
        let mut sum = [0u64; 3];
        let mut count = 0u64;
        for y in (0..frame.height as usize).step_by(step) {
            for x in (0..frame.width as usize).step_by(step) {
                let pixel = (y * frame.width as usize + x) * 3;
                for (channel, total) in sum.iter_mut().enumerate() {
                    *total += frame.rgb[pixel + channel] as u64;
                }
                count += 1;
            }
        }
        [
            sum[0] as f64 / count.max(1) as f64 / 255.0,
            sum[1] as f64 / count.max(1) as f64 / 255.0,
            sum[2] as f64 / count.max(1) as f64 / 255.0,
        ]
    };
    let input = means(source);
    let result = means(output);
    for channel in 0..3 {
        let others = result[(channel + 1) % 3].max(result[(channel + 2) % 3]);
        if input[channel] > 0.02 && result[channel] < 0.003 && others > 0.02 {
            let name = ["red", "green", "blue"][channel];
            return Err(format!(
                "NVIDIA RTX VSR returned a corrupted image: the {name} channel collapsed"
            ));
        }
    }
    Ok(())
}

fn pack_rgba8(source: &Frame) -> Vec<u8> {
    let mut rgba = vec![255u8; source.width as usize * source.height as usize * 4];
    for (source_pixel, target_pixel) in source.rgb.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
        target_pixel[..3].copy_from_slice(source_pixel);
    }
    rgba
}

fn unpack_rgba8(rgba: &[u8], width: u32, height: u32) -> Result<Frame, String> {
    let n = width as usize * height as usize;
    if rgba.len() < n * 4 {
        return Err("NVIDIA RTX VSR returned a short RGBA buffer".into());
    }
    let mut rgb = Vec::with_capacity(n * 3);
    for pixel in rgba.chunks_exact(4).take(n) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    Ok(Frame { width, height, rgb })
}

fn resample_bicubic(src: &Frame, dest_width: u32, dest_height: u32) -> Frame {
    if src.width == dest_width && src.height == dest_height {
        return Frame {
            width: dest_width,
            height: dest_height,
            rgb: src.rgb.clone(),
        };
    }

    let scale_x = (src.width as f32 / dest_width as f32).max(1.0);
    let scale_y = (src.height as f32 / dest_height as f32).max(1.0);
    let mut rgb = vec![0u8; dest_width as usize * dest_height as usize * 3];
    for (y, row) in rgb.chunks_mut(dest_width as usize * 3).enumerate() {
        let src_y = (y as f32 + 0.5) * src.height as f32 / dest_height as f32 - 0.5;
        for (x, pixel) in row.chunks_mut(3).enumerate() {
            let src_x = (x as f32 + 0.5) * src.width as f32 / dest_width as f32 - 0.5;
            let sample = sample_bicubic(src, src_x, src_y, scale_x, scale_y);
            pixel[0] = sample[0].round().clamp(0.0, 255.0) as u8;
            pixel[1] = sample[1].round().clamp(0.0, 255.0) as u8;
            pixel[2] = sample[2].round().clamp(0.0, 255.0) as u8;
        }
    }

    Frame {
        width: dest_width,
        height: dest_height,
        rgb,
    }
}

fn cubic_weight(t: f32) -> f32 {
    let x = t.abs();
    if x <= 1.0 {
        (1.5 * x - 2.5) * x * x + 1.0
    } else if x < 2.0 {
        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
    } else {
        0.0
    }
}

fn sample_bicubic(src: &Frame, x: f32, y: f32, scale_x: f32, scale_y: f32) -> [f32; 3] {
    let max_x = src.width.saturating_sub(1) as i32;
    let max_y = src.height.saturating_sub(1) as i32;
    let ix = x.floor() as i32;
    let iy = y.floor() as i32;
    let fx = x - ix as f32;
    let fy = y - iy as f32;
    let support_x = (2.0 * scale_x).ceil() as i32;
    let support_y = (2.0 * scale_y).ceil() as i32;
    let mut acc = [0.0f32; 3];
    let mut weight_sum = 0.0f32;
    for j in -support_y..=support_y {
        let wy = cubic_weight((fy - j as f32) / scale_y) / scale_y;
        if wy == 0.0 {
            continue;
        }
        let sy = (iy + j).clamp(0, max_y) as usize;
        for i in -support_x..=support_x {
            let wx = cubic_weight((fx - i as f32) / scale_x) / scale_x;
            if wx == 0.0 {
                continue;
            }
            let weight = wx * wy;
            let sx = (ix + i).clamp(0, max_x) as usize;
            let pixel = (sy * src.width as usize + sx) * 3;
            acc[0] += src.rgb[pixel] as f32 * weight;
            acc[1] += src.rgb[pixel + 1] as f32 * weight;
            acc[2] += src.rgb[pixel + 2] as f32 * weight;
            weight_sum += weight;
        }
    }
    if weight_sum <= f32::EPSILON {
        return [0.0, 0.0, 0.0];
    }
    [
        acc[0] / weight_sum,
        acc[1] / weight_sum,
        acc[2] / weight_sum,
    ]
}

#[repr(C)]
#[derive(Default)]
struct NvCvImage {
    width: u32,
    height: u32,
    pitch: i32,
    pixel_format: c_int,
    component_type: c_int,
    pixel_bytes: u8,
    component_bytes: u8,
    num_components: u8,
    planar: u8,
    gpu_mem: u8,
    colorspace: u8,
    reserved: [u8; 2],
    pixels: *mut c_void,
    delete_ptr: *mut c_void,
    delete_proc: Option<unsafe extern "C" fn(*mut c_void)>,
    buffer_bytes: u64,
}

type NvStatus = c_int;
type NvEffect = *mut c_void;
type NvImageInit = unsafe extern "C" fn(
    *mut NvCvImage,
    u32,
    u32,
    c_int,
    *mut c_void,
    c_int,
    c_int,
    u32,
    u32,
) -> NvStatus;
type NvImageAlloc =
    unsafe extern "C" fn(*mut NvCvImage, u32, u32, c_int, c_int, u32, u32, u32) -> NvStatus;
type NvImageDealloc = unsafe extern "C" fn(*mut NvCvImage);
type NvImageTransfer = unsafe extern "C" fn(
    *const NvCvImage,
    *mut NvCvImage,
    f32,
    *mut c_void,
    *mut NvCvImage,
) -> NvStatus;
type NvCreateEffect = unsafe extern "C" fn(*const c_char, *mut NvEffect) -> NvStatus;
type NvDestroyEffect = unsafe extern "C" fn(NvEffect);
type NvSetU32 = unsafe extern "C" fn(NvEffect, *const c_char, u32) -> NvStatus;
type NvSetImage = unsafe extern "C" fn(NvEffect, *const c_char, *mut NvCvImage) -> NvStatus;
type NvSetStream = unsafe extern "C" fn(NvEffect, *const c_char, *mut c_void) -> NvStatus;
type NvLoad = unsafe extern "C" fn(NvEffect) -> NvStatus;
type NvRun = unsafe extern "C" fn(NvEffect, c_int) -> NvStatus;
type NvErrorString = unsafe extern "C" fn(NvStatus) -> *const c_char;

struct Api {
    _libraries: Vec<Library>,
    image_init: NvImageInit,
    image_alloc: NvImageAlloc,
    image_dealloc: NvImageDealloc,
    image_transfer: NvImageTransfer,
    create_effect: NvCreateEffect,
    destroy_effect: NvDestroyEffect,
    set_u32: NvSetU32,
    set_image: NvSetImage,
    set_stream: NvSetStream,
    load_effect: NvLoad,
    run_effect: NvRun,
    error_string: NvErrorString,
}

impl Api {
    unsafe fn load(runtime: &Path) -> Result<Self, String> {
        let mut libraries = Vec::new();
        for name in DLLS {
            libraries.push(
                // SAFETY: the runtime directory is caller-selected and every `Library` is
                // retained in the returned `Api` for all resolved symbol lifetimes.
                unsafe { Library::new(runtime.join(name)) }
                    .map_err(|error| format!("loading NVIDIA RTX VSR {name}: {error}"))?,
            );
        }
        let image = libraries
            .iter()
            .find(|library| unsafe { library.get::<NvImageAlloc>(b"NvCVImage_Alloc\0").is_ok() })
            .ok_or_else(|| "NVCVImage.dll exports were not found".to_string())?;
        let effects = libraries
            .iter()
            .find(|library| unsafe {
                library
                    .get::<NvCreateEffect>(b"NvVFX_CreateEffect\0")
                    .is_ok()
            })
            .ok_or_else(|| "NVVideoEffects.dll exports were not found".to_string())?;

        Ok(Self {
            image_init: *unsafe { image.get(b"NvCVImage_Init\0") }
                .map_err(|error| error.to_string())?,
            image_alloc: *unsafe { image.get(b"NvCVImage_Alloc\0") }
                .map_err(|error| error.to_string())?,
            image_dealloc: *unsafe { image.get(b"NvCVImage_Dealloc\0") }
                .map_err(|error| error.to_string())?,
            image_transfer: *unsafe { image.get(b"NvCVImage_Transfer\0") }
                .map_err(|error| error.to_string())?,
            create_effect: *unsafe { effects.get(b"NvVFX_CreateEffect\0") }
                .map_err(|error| error.to_string())?,
            destroy_effect: *unsafe { effects.get(b"NvVFX_DestroyEffect\0") }
                .map_err(|error| error.to_string())?,
            set_u32: *unsafe { effects.get(b"NvVFX_SetU32\0") }
                .map_err(|error| error.to_string())?,
            set_image: *unsafe { effects.get(b"NvVFX_SetImage\0") }
                .map_err(|error| error.to_string())?,
            set_stream: *unsafe { effects.get(b"NvVFX_SetCudaStream\0") }
                .map_err(|error| error.to_string())?,
            load_effect: *unsafe { effects.get(b"NvVFX_Load\0") }
                .map_err(|error| error.to_string())?,
            run_effect: *unsafe { effects.get(b"NvVFX_Run\0") }
                .map_err(|error| error.to_string())?,
            error_string: *unsafe {
                effects
                    .get(b"NvCV_GetErrorStringFromCode\0")
                    .or_else(|_| image.get(b"NvCV_GetErrorStringFromCode\0"))
            }
            .map_err(|error| error.to_string())?,
            _libraries: libraries,
        })
    }

    fn check(&self, status: NvStatus, operation: &str) -> Result<(), String> {
        if status == 0 {
            return Ok(());
        }
        let detail = unsafe {
            let pointer = (self.error_string)(status);
            if pointer.is_null() {
                "unknown NVIDIA error".into()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            }
        };
        Err(format!("{operation}: {detail} (code {status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_runtime_is_complete_when_present() {
        let workspace_runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("rtx-vsr");
        if !workspace_runtime.exists() {
            return;
        }
        let missing: Vec<_> = DLLS
            .iter()
            .filter(|name| !workspace_runtime.join(name).is_file())
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "incomplete local RTX VSR runtime: {missing:?}"
        );
        assert!(
            available(),
            "complete local RTX VSR runtime was not discovered"
        );
    }

    #[test]
    fn quality_indices_default_to_ultra() {
        assert_eq!(Quality::from_index(0), Quality::Low);
        assert_eq!(Quality::from_index(2), Quality::High);
        assert_eq!(Quality::from_index(99), Quality::Ultra);
    }

    #[test]
    fn named_goal_asks_nvidia_for_exact_scale() {
        let plan = plan_output(720, 1280, 1080, 1920).expect("1.5x plan");
        assert_eq!(
            (plan.vsr_width, plan.vsr_height, plan.uses_hybrid()),
            (1152, 1920, true)
        );
    }

    #[test]
    fn portrait_720_pads_width_and_keeps_3x() {
        assert_eq!(align_width(720), 768);
        assert_eq!(align_width(1280), 1280);
        let plan = plan_output(720, 1280, 2160, 3840).expect("portrait 4K plan");
        assert_eq!((plan.vsr_width, plan.vsr_height), (2304, 3840));
        assert!(plan.uses_hybrid());
    }

    #[test]
    fn four_k_from_720p_uses_exact_vsr() {
        let plan = plan_output(1280, 720, 3840, 2160).expect("4K plan");
        assert_eq!((plan.vsr_width, plan.vsr_height), (3840, 2160));
        assert!(!plan.uses_hybrid());
    }

    #[test]
    fn protected_intermediate_alignment_does_not_exceed_limit() {
        let plan = plan_output(4096, 4096, 16_384, 16_384).expect("16K hybrid");
        assert_eq!((plan.vsr_width, plan.vsr_height), (15_360, 15_360));
        assert!(plan.uses_hybrid());
        assert_eq!(
            align_to_down(9_216.0 * 15_360.0 / 16_384.0, ALIGNMENT),
            8_640
        );
    }

    #[test]
    fn verified_edge_uses_exact_vsr() {
        let plan = plan_output(3840, 3840, 15_360, 15_360).expect("4x request");
        assert_eq!((plan.vsr_width, plan.vsr_height), (15_360, 15_360));
        assert!(!plan.uses_hybrid());
    }

    #[test]
    fn rejects_oversize_and_downscale() {
        assert!(plan_output(64, 64, 16_385, 16_385).is_err());
        assert!(plan_output(1920, 1080, 1280, 720).is_err());
        assert!(plan_output(320, 180, 3840, 2160).is_err());
    }

    #[test]
    fn bicubic_identity_and_upscale_keep_signal() {
        let source = Frame {
            width: 2,
            height: 2,
            rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
        };
        let same = resample_bicubic(&source, 2, 2);
        assert_eq!(same.rgb, source.rgb);
        let up = resample_bicubic(&source, 4, 4);
        assert_eq!((up.width, up.height), (4, 4));
        assert!(up.rgb.iter().any(|value| *value > 0));
    }
}
