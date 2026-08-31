//! Retained monitor scaler that opts into libswscale's frame API for large frames.

use std::{ffi::c_void, thread};

use ffmpeg::{
    format::Pixel,
    software::scaling::{context::Context as LegacyScalingContext, flag::Flags},
    util::frame::video::Video,
};
use ffmpeg_next as ffmpeg;

const LARGE_FRAME_AREA: u64 = 1_920 * 1_080;
const THREAD_CAP: u32 = 2;
// Four foreground sources can each use two scaler threads on this CPU floor.
// This per-context cap is not a global CPU reservation. Sustained playback on
// other hosts still needs qualification; small/low-core workloads stay serial.
const MIN_PARALLEL_CPUS: usize = 8;

/// Keeps the established `sws_scale` path as the conservative fallback.
pub(crate) struct ScalingContext {
    backend: ScalerBackend,
    input: ScalerDefinition,
    output: ScalerDefinition,
}

enum ScalerBackend {
    Serial(LegacyScalingContext),
    Threaded(RawSwsContext),
}

#[derive(Clone, Copy)]
struct ScalerDefinition {
    format: Pixel,
    width: u32,
    height: u32,
}

impl ScalingContext {
    pub(crate) fn get(
        src_format: Pixel,
        src_width: u32,
        src_height: u32,
        dst_format: Pixel,
        dst_width: u32,
        dst_height: u32,
        flags: Flags,
    ) -> Result<Self, ffmpeg::Error> {
        Self::with_threads(
            src_format,
            src_width,
            src_height,
            dst_format,
            dst_width,
            dst_height,
            flags,
            selected_threads(src_width, src_height, dst_width, dst_height),
            false,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn get_with_forced_threads(
        src_format: Pixel,
        src_width: u32,
        src_height: u32,
        dst_format: Pixel,
        dst_width: u32,
        dst_height: u32,
        flags: Flags,
        threads: u32,
    ) -> Result<Self, ffmpeg::Error> {
        Self::with_threads(
            src_format,
            src_width,
            src_height,
            dst_format,
            dst_width,
            dst_height,
            flags,
            threads.min(THREAD_CAP),
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_threads(
        src_format: Pixel,
        src_width: u32,
        src_height: u32,
        dst_format: Pixel,
        dst_width: u32,
        dst_height: u32,
        flags: Flags,
        threads: u32,
        require_raw: bool,
    ) -> Result<Self, ffmpeg::Error> {
        let backend = if threads >= 2 {
            // Threading is an optional optimization. A raw-context setup failure must preserve
            // the historical libswscale context and its pixels.
            match RawSwsContext::new(
                ScalerDefinition {
                    format: src_format,
                    width: src_width,
                    height: src_height,
                },
                ScalerDefinition {
                    format: dst_format,
                    width: dst_width,
                    height: dst_height,
                },
                flags,
                threads,
            ) {
                Ok(raw) => ScalerBackend::Threaded(raw),
                Err(error) if require_raw => return Err(error),
                Err(_) => ScalerBackend::Serial(LegacyScalingContext::get(
                    src_format, src_width, src_height, dst_format, dst_width, dst_height, flags,
                )?),
            }
        } else {
            ScalerBackend::Serial(LegacyScalingContext::get(
                src_format, src_width, src_height, dst_format, dst_width, dst_height, flags,
            )?)
        };
        Ok(Self {
            backend,
            input: ScalerDefinition {
                format: src_format,
                width: src_width,
                height: src_height,
            },
            output: ScalerDefinition {
                format: dst_format,
                width: dst_width,
                height: dst_height,
            },
        })
    }

    /// Selects the currently active context for per-frame color configuration.
    pub(crate) unsafe fn as_mut_ptr(&mut self) -> *mut ffmpeg::ffi::SwsContext {
        match &mut self.backend {
            ScalerBackend::Threaded(raw) => raw.as_mut_ptr(),
            // SAFETY: this wrapper exclusively owns the retained legacy context.
            ScalerBackend::Serial(legacy) => unsafe { legacy.as_mut_ptr() },
        }
    }

    pub(crate) fn run(&mut self, input: &Video, output: &mut Video) -> Result<(), ffmpeg::Error> {
        if input.format() != self.input.format
            || input.width() != self.input.width
            || input.height() != self.input.height
        {
            return Err(ffmpeg::Error::InputChanged);
        }
        match &mut self.backend {
            ScalerBackend::Threaded(raw) => raw.run(input, output, self.output),
            ScalerBackend::Serial(legacy) => legacy.run(input, output),
        }
    }

    #[cfg(test)]
    pub(crate) fn selected_thread_count(&self) -> u32 {
        match &self.backend {
            ScalerBackend::Serial(_) => 1,
            ScalerBackend::Threaded(raw) => {
                let mut value = 0_i64;
                // SAFETY: the initialized context exposes this integer AVOption.
                let result = unsafe {
                    ffmpeg::ffi::av_opt_get_int(
                        raw.ptr.cast::<c_void>(),
                        c"threads".as_ptr(),
                        0,
                        &mut value,
                    )
                };
                assert!(result >= 0, "read initialized scaler thread count");
                u32::try_from(value).expect("valid initialized scaler thread count")
            }
        }
    }
}

fn selected_threads(src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> u32 {
    let available = thread::available_parallelism().map_or(1, |count| count.get());
    thread_policy(src_width, src_height, dst_width, dst_height, available)
}

fn thread_policy(
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    available: usize,
) -> u32 {
    let src_area = u64::from(src_width) * u64::from(src_height);
    let dst_area = u64::from(dst_width) * u64::from(dst_height);
    if src_area >= LARGE_FRAME_AREA
        && dst_area >= LARGE_FRAME_AREA
        && available >= MIN_PARALLEL_CPUS
    {
        THREAD_CAP
    } else {
        1
    }
}

struct RawSwsContext {
    ptr: *mut ffmpeg::ffi::SwsContext,
}

impl RawSwsContext {
    fn new(
        input: ScalerDefinition,
        output: ScalerDefinition,
        flags: Flags,
        threads: u32,
    ) -> Result<Self, ffmpeg::Error> {
        // SAFETY: allocation returns either a context owned by this RAII wrapper or null.
        let ptr = unsafe { ffmpeg::ffi::sws_alloc_context() };
        if ptr.is_null() {
            return Err(ffmpeg::Error::InvalidData);
        }
        let raw = Self { ptr };
        raw.set_int(b"srcw\0", i64::from(input.width))?;
        raw.set_int(b"srch\0", i64::from(input.height))?;
        raw.set_int(
            b"src_format\0",
            i64::from(ffmpeg::ffi::AVPixelFormat::from(input.format) as i32),
        )?;
        raw.set_int(b"dstw\0", i64::from(output.width))?;
        raw.set_int(b"dsth\0", i64::from(output.height))?;
        raw.set_int(
            b"dst_format\0",
            i64::from(ffmpeg::ffi::AVPixelFormat::from(output.format) as i32),
        )?;
        raw.set_int(b"sws_flags\0", i64::from(flags.bits()))?;
        raw.set_int(b"threads\0", i64::from(threads))?;
        // SAFETY: all required AVOptions are set and no custom filters are used.
        let result = unsafe {
            ffmpeg::ffi::sws_init_context(ptr, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if result < 0 {
            return Err(ffmpeg::Error::from(result));
        }
        Ok(raw)
    }

    fn set_int(&self, name: &'static [u8], value: i64) -> Result<(), ffmpeg::Error> {
        // SAFETY: `ptr` remains owned by self and option names are NUL-terminated literals.
        let result = unsafe {
            ffmpeg::ffi::av_opt_set_int(self.ptr.cast::<c_void>(), name.as_ptr().cast(), value, 0)
        };
        if result < 0 {
            Err(ffmpeg::Error::from(result))
        } else {
            Ok(())
        }
    }

    fn as_mut_ptr(&mut self) -> *mut ffmpeg::ffi::SwsContext {
        self.ptr
    }

    fn run(
        &mut self,
        input: &Video,
        output: &mut Video,
        expected_output: ScalerDefinition,
    ) -> Result<(), ffmpeg::Error> {
        // SAFETY: both handles are valid AVFrame pointers for the duration of this call.
        if unsafe { std::ptr::eq(input.as_ptr(), output.as_mut_ptr()) } {
            return Err(ffmpeg::Error::InvalidData);
        }
        // Reject mismatched preallocated output before allocating or copying anything.
        if !unsafe { output.is_empty() }
            && (output.format() != expected_output.format
                || output.width() != expected_output.width
                || output.height() != expected_output.height)
        {
            return Err(ffmpeg::Error::OutputChanged);
        }
        // SAFETY: Video owns a valid AVFrame. The checked allocation initializes a fresh,
        // refcounted buffer before libswscale receives its pointer.
        if unsafe { output.is_empty() } {
            output.set_format(expected_output.format);
            output.set_width(expected_output.width);
            output.set_height(expected_output.height);
            let result = unsafe { ffmpeg::ffi::av_frame_get_buffer(output.as_mut_ptr(), 32) };
            if result < 0 {
                return Err(ffmpeg::Error::from(result));
            }
        } else {
            // SAFETY: libavutil ensures this frame's backing buffers are uniquely writable or
            // clones them before the frame API writes output pixels.
            let result = unsafe { ffmpeg::ffi::av_frame_make_writable(output.as_mut_ptr()) };
            if result < 0 {
                return Err(ffmpeg::Error::from(result));
            }
        }
        // SAFETY: dimensions/formats were validated against the initialized raw context. Both
        // independently-owned AVFrames remain live and non-aliased for this synchronous call.
        let result =
            unsafe { ffmpeg::ffi::sws_scale_frame(self.ptr, output.as_mut_ptr(), input.as_ptr()) };
        if result < 0 {
            Err(ffmpeg::Error::from(result))
        } else {
            Ok(())
        }
    }
}

impl Drop for RawSwsContext {
    fn drop(&mut self) {
        // SAFETY: `ptr` is either a context returned by sws_alloc_context or has been freed only
        // here; libswscale accepts partially initialized contexts.
        unsafe { ffmpeg::ffi::sws_freeContext(self.ptr) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_policy_requires_both_full_hd_areas() {
        assert_eq!(thread_policy(1_920, 1_080, 1_919, 1_080, 8), 1);
        assert_eq!(thread_policy(1_919, 1_080, 1_920, 1_080, 8), 1);
        for cpus in [0, 1, 2, 3, 4, 7] {
            assert_eq!(thread_policy(1_920, 1_080, 1_920, 1_080, cpus), 1);
        }
        assert_eq!(thread_policy(1_920, 1_080, 1_920, 1_080, 8), 2);
        assert_eq!(thread_policy(1_920, 1_080, 1_920, 1_080, 28), 2);
        assert_eq!(thread_policy(4_096, 4_096, 4_096, 4_096, 8), 2);
    }

    #[test]
    fn forced_threads_are_capped() {
        let serial = ScalingContext::get_with_forced_threads(
            Pixel::RGBA,
            8,
            8,
            Pixel::RGBA,
            8,
            8,
            Flags::BILINEAR,
            0,
        )
        .unwrap();
        let threaded = ScalingContext::get_with_forced_threads(
            Pixel::RGBA,
            8,
            8,
            Pixel::RGBA,
            8,
            8,
            Flags::BILINEAR,
            4,
        )
        .unwrap();
        assert_eq!(serial.selected_thread_count(), 1);
        assert_eq!(threaded.selected_thread_count(), THREAD_CAP);
    }

    #[test]
    fn threaded_rejects_changed_frames_and_recovers() {
        let mut scaler = ScalingContext::get_with_forced_threads(
            Pixel::RGBA,
            16,
            16,
            Pixel::RGBA,
            16,
            16,
            Flags::BICUBIC,
            2,
        )
        .unwrap();
        let mut input = Video::new(Pixel::RGBA, 16, 16);
        input.data_mut(0).fill(93);
        let wrong_input = Video::new(Pixel::RGBA, 8, 16);
        let mut output = Video::empty();
        assert_eq!(
            scaler.run(&wrong_input, &mut output),
            Err(ffmpeg::Error::InputChanged)
        );
        assert!(unsafe { output.is_empty() });
        let mut wrong_output = Video::new(Pixel::BGRA, 16, 16);
        wrong_output.data_mut(0).fill(27);
        assert_eq!(
            scaler.run(&input, &mut wrong_output),
            Err(ffmpeg::Error::OutputChanged)
        );
        assert!(wrong_output.data(0).iter().all(|byte| *byte == 27));
        scaler.run(&input, &mut output).unwrap();
        assert!(output.data(0).iter().all(|byte| *byte == 93));
    }

    #[test]
    fn threaded_detaches_shared_output_buffers() {
        let mut scaler = ScalingContext::get_with_forced_threads(
            Pixel::RGBA,
            16,
            16,
            Pixel::RGBA,
            16,
            16,
            Flags::BICUBIC,
            2,
        )
        .unwrap();
        let mut input = Video::new(Pixel::RGBA, 16, 16);
        input.data_mut(0).fill(93);
        let mut original = Video::new(Pixel::RGBA, 16, 16);
        original.data_mut(0).fill(27);
        let mut output = Video::empty();
        // SAFETY: both AVFrames are live, destination is empty; av_frame_ref makes
        // deliberately shared output buffers to exercise copy-on-write behavior.
        assert!(unsafe { ffmpeg::ffi::av_frame_ref(output.as_mut_ptr(), original.as_ptr()) } >= 0);
        scaler.run(&input, &mut output).unwrap();
        assert!(original.data(0).iter().all(|byte| *byte == 27));
        assert!(output.data(0).iter().all(|byte| *byte == 93));
    }

    #[test]
    fn threaded_setup_failure_leaves_later_contexts_usable() {
        for _ in 0..8 {
            assert!(
                ScalingContext::get_with_forced_threads(
                    Pixel::None,
                    16,
                    16,
                    Pixel::RGBA,
                    16,
                    16,
                    Flags::BICUBIC,
                    2,
                )
                .is_err()
            );
        }
        let mut scaler = ScalingContext::get_with_forced_threads(
            Pixel::RGBA,
            16,
            16,
            Pixel::RGBA,
            16,
            16,
            Flags::BICUBIC,
            2,
        )
        .unwrap();
        let mut input = Video::new(Pixel::RGBA, 16, 16);
        input.data_mut(0).fill(0);
        scaler.run(&input, &mut Video::empty()).unwrap();
    }
}
