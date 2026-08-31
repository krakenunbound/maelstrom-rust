use super::*;
use ffmpeg::util::color::{Range, Space};
use std::{ffi::c_void, time::Instant};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 90;

fn patterned_luma(x: u32, y: u32) -> u8 {
    (16 + ((x.wrapping_mul(13) ^ y.wrapping_mul(7)) % 220)) as u8
}

fn patterned_chroma_u(x: u32, y: u32) -> u8 {
    match ((x / 5) + (y / 7)) % 4 {
        0 => 24,
        1 => 232,
        2 => 80,
        _ => 176,
    }
}

fn patterned_chroma_v(x: u32, y: u32) -> u8 {
    match ((x / 7) + (y / 5)) % 4 {
        0 => 232,
        1 => 48,
        2 => 192,
        _ => 96,
    }
}

fn apply_bt709_limited_metadata(frame: &mut Video) {
    frame.set_color_space(Space::BT709);
    frame.set_color_range(Range::MPEG);
}

fn patterned_yuv420p(width: u32, height: u32) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, width, height);
    let luma_stride = frame.stride(0);
    let luma = frame.data_mut(0);
    for y in 0..height as usize {
        for x in 0..width as usize {
            luma[y * luma_stride + x] = patterned_luma(x as u32, y as u32);
        }
    }
    for plane in [1, 2] {
        let chroma_stride = frame.stride(plane);
        let chroma = frame.data_mut(plane);
        for y in 0..height as usize / 2 {
            for x in 0..width as usize / 2 {
                chroma[y * chroma_stride + x] = if plane == 1 {
                    patterned_chroma_u(x as u32, y as u32)
                } else {
                    patterned_chroma_v(x as u32, y as u32)
                };
            }
        }
    }
    apply_bt709_limited_metadata(&mut frame);
    frame
}

fn patterned_nv12(width: u32, height: u32) -> Video {
    let mut frame = Video::new(Pixel::NV12, width, height);
    let luma_stride = frame.stride(0);
    let luma = frame.data_mut(0);
    for y in 0..height as usize {
        for x in 0..width as usize {
            luma[y * luma_stride + x] = patterned_luma(x as u32, y as u32);
        }
    }
    let chroma_stride = frame.stride(1);
    let chroma = frame.data_mut(1);
    for y in 0..height as usize / 2 {
        for x in 0..width as usize / 2 {
            let offset = y * chroma_stride + x * 2;
            chroma[offset] = patterned_chroma_u(x as u32, y as u32);
            chroma[offset + 1] = patterned_chroma_v(x as u32, y as u32);
        }
    }
    apply_bt709_limited_metadata(&mut frame);
    frame
}

fn scale_layout_once(
    frame: &Video,
    output_size: (u32, u32),
    high_quality: bool,
    timings: &DecoderStageTimingAccumulators,
) -> Vec<u8> {
    let (context, mut scaled_size) = StickyMonitor::make_scaler(
        frame.format(),
        frame.width(),
        frame.height(),
        output_size.0,
        output_size.1,
        high_quality,
    )
    .expect("create synthetic layout scaler");
    let mut scaler = Some(context);
    let mut scaler_input = Some((frame.format(), frame.width(), frame.height()));
    let mut scaler_high_quality = Some(high_quality);
    let rgba = scale_monitor_frame(
        &mut scaler,
        &mut scaler_input,
        &mut scaler_high_quality,
        &mut scaled_size,
        frame,
        false,
        output_size,
        high_quality,
        timings,
    )
    .expect("scale synthetic layout frame");
    copy_rgba_frame(&rgba, output_size.0, output_size.1).expect("copy active RGBA pixels")
}

fn assert_active_rgba_rows_equal(label: &str, left: &[u8], right: &[u8], width: u32, height: u32) {
    let row_bytes = width as usize * 4;
    assert_eq!(left.len(), row_bytes * height as usize, "{label} left size");
    assert_eq!(
        right.len(),
        row_bytes * height as usize,
        "{label} right size"
    );
    for row in 0..height as usize {
        assert_eq!(
            &left[row * row_bytes..(row + 1) * row_bytes],
            &right[row * row_bytes..(row + 1) * row_bytes],
            "{label} active RGBA row {row}"
        );
    }
}

#[test]
fn scaler_layout_yuv420p_and_nv12_match_for_native_and_downscaled_rgba() {
    let planar = patterned_yuv420p(WIDTH, HEIGHT);
    let nv12 = patterned_nv12(WIDTH, HEIGHT);
    for high_quality in [false, true] {
        for output_size in [(WIDTH, HEIGHT), (80, 45)] {
            let timings = DecoderStageTimingAccumulators::default();
            let planar_rgba = scale_layout_once(&planar, output_size, high_quality, &timings);
            let nv12_rgba = scale_layout_once(&nv12, output_size, high_quality, &timings);
            assert_active_rgba_rows_equal(
                &format!("{output_size:?}, high_quality={high_quality}"),
                &planar_rgba,
                &nv12_rgba,
                output_size.0,
                output_size.1,
            );
        }
    }
}

struct RetainedScaler {
    scaler: Option<ScalingContext>,
    input: Option<(Pixel, u32, u32)>,
    high_quality: Option<bool>,
    scaled_size: (u32, u32),
}

impl RetainedScaler {
    fn new(frame: &Video, output_size: (u32, u32), high_quality: bool) -> Self {
        let (scaler, scaled_size) = StickyMonitor::make_scaler(
            frame.format(),
            frame.width(),
            frame.height(),
            output_size.0,
            output_size.1,
            high_quality,
        )
        .expect("create full-HD synthetic layout scaler");
        Self {
            scaler: Some(scaler),
            input: Some((frame.format(), frame.width(), frame.height())),
            high_quality: Some(high_quality),
            scaled_size,
        }
    }

    fn scale(
        &mut self,
        frame: &Video,
        output_size: (u32, u32),
        high_quality: bool,
        timings: &DecoderStageTimingAccumulators,
    ) -> Vec<u8> {
        let rgba = scale_monitor_frame(
            &mut self.scaler,
            &mut self.input,
            &mut self.high_quality,
            &mut self.scaled_size,
            frame,
            false,
            output_size,
            high_quality,
            timings,
        )
        .expect("scale full-HD synthetic layout frame");
        copy_rgba_frame(&rgba, output_size.0, output_size.1).expect("copy active full-HD RGBA")
    }
}

fn percentile_ms(samples: &mut [u64], percentile: usize) -> f64 {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percentile / 100] as f64 / 1_000_000.0
}

struct RawSwsContext(*mut ffmpeg::ffi::SwsContext);

// Keep the pre-threading implementation as an independent pixel oracle. In
// particular, do not compare two instances of the new production wrapper.
struct LegacyScaler(ffmpeg::software::scaling::context::Context);

impl LegacyScaler {
    fn new(frame: &Video, output_size: (u32, u32), high_quality: bool) -> Self {
        Self(
            ffmpeg::software::scaling::context::Context::get(
                frame.format(),
                frame.width(),
                frame.height(),
                Pixel::RGBA,
                output_size.0,
                output_size.1,
                scaling_flags(high_quality),
            )
            .expect("create original single-slice scaler"),
        )
    }

    fn scale(
        &mut self,
        frame: &Video,
        output_size: (u32, u32),
        _high_quality: bool,
        _timings: &DecoderStageTimingAccumulators,
    ) -> Vec<u8> {
        // These test cases explicitly declare their matrix/range. The independent
        // legacy converter receives those same neutral conversion coefficients.
        let matrix = match frame.color_space() {
            Space::BT709 => ffmpeg::ffi::SWS_CS_ITU709,
            Space::BT2020NCL => ffmpeg::ffi::SWS_CS_BT2020,
            _ => ffmpeg::ffi::SWS_CS_ITU601,
        };
        // SAFETY: the legacy context is exclusively owned and the table is static.
        unsafe {
            let coefficients = ffmpeg::ffi::sws_getCoefficients(matrix);
            assert!(
                ffmpeg::ffi::sws_setColorspaceDetails(
                    self.0.as_mut_ptr(),
                    coefficients,
                    i32::from(frame.color_range() == Range::JPEG),
                    coefficients,
                    1,
                    0,
                    1 << 16,
                    1 << 16
                ) >= 0
            );
        }
        let mut rgba = Video::empty();
        self.0
            .run(frame, &mut rgba)
            .expect("run original single-slice scaler");
        copy_rgba_frame(&rgba, output_size.0, output_size.1).expect("copy original RGBA")
    }
}

impl RawSwsContext {
    fn new(
        frame: &Video,
        output_size: (u32, u32),
        high_quality: bool,
        threads: i64,
    ) -> Result<Self, String> {
        // SAFETY: `sws_alloc_context` creates an uninitialized context. All required
        // AVOptions are set before initialization; Drop releases it on every path.
        let context = unsafe { ffmpeg::ffi::sws_alloc_context() };
        if context.is_null() {
            return Err("could not allocate raw SwsContext".into());
        }
        let raw = Self(context);
        let set_int = |name: &'static [u8], value: i64| {
            // SAFETY: `context` is live and the option names are NUL-terminated.
            let result = unsafe {
                ffmpeg::ffi::av_opt_set_int(
                    context.cast::<c_void>(),
                    name.as_ptr().cast(),
                    value,
                    0,
                )
            };
            if result < 0 {
                Err(format!(
                    "could not set raw scaler option {}: {}",
                    String::from_utf8_lossy(&name[..name.len() - 1]),
                    ffmpeg::Error::from(result)
                ))
            } else {
                Ok(())
            }
        };
        let flags = scaling_flags(high_quality).bits() as i64;
        let src_format: ffmpeg::ffi::AVPixelFormat = frame.format().into();
        let dst_format: ffmpeg::ffi::AVPixelFormat = Pixel::RGBA.into();
        set_int(b"srcw\0", frame.width().into())?;
        set_int(b"srch\0", frame.height().into())?;
        set_int(b"src_format\0", i64::from(src_format as i32))?;
        set_int(b"dstw\0", output_size.0.into())?;
        set_int(b"dsth\0", output_size.1.into())?;
        set_int(b"dst_format\0", i64::from(dst_format as i32))?;
        set_int(b"sws_flags\0", flags)?;
        set_int(b"threads\0", threads)?;

        // SAFETY: no filter is required and all context options were set above.
        let initialized = unsafe {
            ffmpeg::ffi::sws_init_context(context, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if initialized < 0 {
            return Err(format!(
                "could not initialize raw scaler: {}",
                ffmpeg::Error::from(initialized)
            ));
        }
        Ok(raw)
    }

    fn configure_bt709_limited(&mut self) -> Result<(), String> {
        // SAFETY: this initialized context is exclusively owned by `self`; the
        // coefficient table is static and RGBA output is full-range.
        let result = unsafe {
            let coefficients = ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_ITU709);
            ffmpeg::ffi::sws_setColorspaceDetails(
                self.0,
                coefficients,
                0,
                coefficients,
                1,
                0,
                1 << 16,
                1 << 16,
            )
        };
        if result < 0 {
            Err(format!(
                "could not configure raw BT.709 limited conversion: {}",
                ffmpeg::Error::from(result)
            ))
        } else {
            Ok(())
        }
    }

    fn scale_frame(&mut self, input: &Video, output: &mut Video) -> Result<(), String> {
        // SAFETY: the initialized context dimensions/formats match both refcounted
        // frames, which remain valid for the duration of the synchronous call.
        let result =
            unsafe { ffmpeg::ffi::sws_scale_frame(self.0, output.as_mut_ptr(), input.as_ptr()) };
        if result < 0 {
            Err(format!(
                "raw sws_scale_frame failed: {}",
                ffmpeg::Error::from(result)
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for RawSwsContext {
    fn drop(&mut self) {
        // SAFETY: libswscale accepts the context returned by `sws_alloc_context`.
        unsafe { ffmpeg::ffi::sws_freeContext(self.0) };
    }
}

fn threaded_convert(
    scaler: &mut RawSwsContext,
    frame: &Video,
    output_size: (u32, u32),
) -> Result<(Video, u64), String> {
    let started = Instant::now();
    // Match production conversion ownership: allocate a fresh refcounted RGBA frame
    // and configure color conversion for each frame.
    let mut rgba = Video::new(Pixel::RGBA, output_size.0, output_size.1);
    scaler.configure_bt709_limited()?;
    scaler.scale_frame(frame, &mut rgba)?;
    Ok((rgba, started.elapsed().as_nanos() as u64))
}

fn verify_threaded_layouts(
    planar: &Video,
    nv12: &Video,
    output_size: (u32, u32),
    high_quality: bool,
) {
    let timings = DecoderStageTimingAccumulators::default();
    let mut planar_baseline = LegacyScaler::new(planar, output_size, high_quality);
    let mut nv12_baseline = LegacyScaler::new(nv12, output_size, high_quality);
    for (layout, frame, baseline) in [
        ("yuv420p", planar, &mut planar_baseline),
        ("nv12", nv12, &mut nv12_baseline),
    ] {
        let expected = baseline.scale(frame, output_size, high_quality, &timings);
        for threads in [1, 2, 4] {
            let mut threaded = RawSwsContext::new(frame, output_size, high_quality, threads)
                .unwrap_or_else(|error| panic!("{layout} threads={threads}: {error}"));
            let (rgba, _) = threaded_convert(&mut threaded, frame, output_size)
                .unwrap_or_else(|error| panic!("{layout} threads={threads}: {error}"));
            let actual = copy_rgba_frame(&rgba, output_size.0, output_size.1)
                .unwrap_or_else(|error| panic!("{layout} threads={threads}: {error}"));
            assert_active_rgba_rows_equal(
                &format!("{layout} threads={threads} {output_size:?} high_quality={high_quality}"),
                &actual,
                &expected,
                output_size.0,
                output_size.1,
            );
        }
    }
}

#[test]
#[ignore = "full-HD synthetic scaler timing diagnostic"]
fn scaler_layout_full_hd_timing() {
    let output_size = (1_920, 1_080);
    let planar = patterned_yuv420p(output_size.0, output_size.1);
    let nv12 = patterned_nv12(output_size.0, output_size.1);
    for high_quality in [false, true] {
        let timings = DecoderStageTimingAccumulators::default();
        let mut planar_scaler = RetainedScaler::new(&planar, output_size, high_quality);
        let mut nv12_scaler = RetainedScaler::new(&nv12, output_size, high_quality);
        for (layout, frame, scaler) in [
            ("yuv420p", &planar, &mut planar_scaler),
            ("nv12", &nv12, &mut nv12_scaler),
        ] {
            // Time each layout against its own stable output so this diagnostic
            // can also measure the old implementation that fails the parity test.
            let expected = scaler.scale(frame, output_size, high_quality, &timings);
            for _ in 0..8 {
                let rgba = scaler.scale(frame, output_size, high_quality, &timings);
                assert_active_rgba_rows_equal(
                    "full-HD warmup",
                    &rgba,
                    &expected,
                    output_size.0,
                    output_size.1,
                );
            }
            let mut samples = Vec::with_capacity(120);
            for _ in 0..120 {
                let before = timings.snapshot().scaler.total_nanos;
                let rgba = scaler.scale(frame, output_size, high_quality, &timings);
                let after = timings.snapshot().scaler.total_nanos;
                let scaler_nanos = after.saturating_sub(before);
                assert!(scaler_nanos > 0, "{layout} scaler timing");
                assert_eq!(
                    rgba.len(),
                    output_size.0 as usize * output_size.1 as usize * 4
                );
                assert_active_rgba_rows_equal(
                    "full-HD timed output",
                    &rgba,
                    &expected,
                    output_size.0,
                    output_size.1,
                );
                samples.push(scaler_nanos);
            }
            let p50 = percentile_ms(&mut samples.clone(), 50);
            let p95 = percentile_ms(&mut samples, 95);
            eprintln!(
                "full-HD {layout} high_quality={high_quality}: p50={p50:.3}ms p95={p95:.3}ms"
            );
        }
    }
}

#[test]
#[ignore = "full-HD threaded raw SwsContext timing prototype"]
fn scaler_layout_threaded_probe() {
    let requested_size = (1_920, 1_080);
    let planar = patterned_yuv420p(requested_size.0, requested_size.1);
    let nv12 = patterned_nv12(requested_size.0, requested_size.1);
    let odd_downscaled_size = fitted_size(planar.width(), planar.height(), 959, 539);
    for high_quality in [false, true] {
        verify_threaded_layouts(&planar, &nv12, odd_downscaled_size, high_quality);
        verify_threaded_layouts(&planar, &nv12, requested_size, high_quality);
        let timings = DecoderStageTimingAccumulators::default();
        let mut planar_baseline = LegacyScaler::new(&planar, requested_size, high_quality);
        let mut nv12_baseline = LegacyScaler::new(&nv12, requested_size, high_quality);
        for (layout, frame, baseline) in [
            ("yuv420p", &planar, &mut planar_baseline),
            ("nv12", &nv12, &mut nv12_baseline),
        ] {
            let expected = baseline.scale(frame, requested_size, high_quality, &timings);
            for threads in [1, 2, 4] {
                let init_started = Instant::now();
                let mut threaded = RawSwsContext::new(frame, requested_size, high_quality, threads)
                    .unwrap_or_else(|error| panic!("{layout} threads={threads}: {error}"));
                let init_ms = init_started.elapsed().as_secs_f64() * 1_000.0;
                for _ in 0..8 {
                    let (rgba, _) = threaded_convert(&mut threaded, frame, requested_size)
                        .expect("threaded full-HD warmup");
                    let actual = copy_rgba_frame(&rgba, requested_size.0, requested_size.1)
                        .expect("copy threaded full-HD warmup");
                    assert_active_rgba_rows_equal(
                        "threaded full-HD warmup",
                        &actual,
                        &expected,
                        requested_size.0,
                        requested_size.1,
                    );
                }
                let mut conversion_samples = Vec::with_capacity(80);
                let mut end_to_end_samples = Vec::with_capacity(80);
                for _ in 0..80 {
                    let end_to_end_started = Instant::now();
                    let (rgba, conversion_nanos) =
                        threaded_convert(&mut threaded, frame, requested_size)
                            .expect("threaded full-HD timed conversion");
                    let actual = copy_rgba_frame(&rgba, requested_size.0, requested_size.1)
                        .expect("copy threaded full-HD timed conversion");
                    let end_to_end_nanos = end_to_end_started.elapsed().as_nanos() as u64;
                    assert_active_rgba_rows_equal(
                        "threaded full-HD timed output",
                        &actual,
                        &expected,
                        requested_size.0,
                        requested_size.1,
                    );
                    conversion_samples.push(conversion_nanos);
                    end_to_end_samples.push(end_to_end_nanos);
                }
                let conversion_p50 = percentile_ms(&mut conversion_samples.clone(), 50);
                let conversion_p95 = percentile_ms(&mut conversion_samples, 95);
                let end_to_end_p50 = percentile_ms(&mut end_to_end_samples.clone(), 50);
                let end_to_end_p95 = percentile_ms(&mut end_to_end_samples, 95);
                eprintln!(
                    "threaded full-HD {layout} threads={threads} high_quality={high_quality}: \
                     init={init_ms:.3}ms; conversion p50={conversion_p50:.3}ms \
                     p95={conversion_p95:.3}ms; end-to-end p50={end_to_end_p50:.3}ms \
                     p95={end_to_end_p95:.3}ms"
                );
            }
        }
    }
}

fn patterned_format(format: Pixel, width: u32, height: u32) -> Video {
    let mut frame = Video::new(format, width, height);
    for plane in 0..frame.planes() {
        let data = frame.data_mut(plane);
        if matches!(
            format,
            Pixel::YUV420P10LE | Pixel::YUV422P10LE | Pixel::YUV444P10LE | Pixel::P010LE
        ) {
            for (index, sample) in data.chunks_exact_mut(2).enumerate() {
                let value = ((index * 37 + plane * 157) % 1024) as u16;
                let value = if format == Pixel::P010LE {
                    value << 6
                } else {
                    value
                };
                sample.copy_from_slice(&value.to_le_bytes());
            }
        } else {
            for (index, sample) in data.iter_mut().enumerate() {
                *sample = ((index * 13 + plane * 71) % 256) as u8;
            }
        }
    }
    apply_bt709_limited_metadata(&mut frame);
    frame
}

#[test]
fn threaded_monitor_matches_legacy_pixels_across_formats_sizes_and_metadata_changes() {
    let timings = DecoderStageTimingAccumulators::default();
    let formats = [
        Pixel::YUV420P,
        Pixel::NV12,
        Pixel::YUV420P10LE,
        Pixel::P010LE,
        Pixel::YUV422P10LE,
        Pixel::YUV444P10LE,
        Pixel::YUVA420P,
        Pixel::RGBA,
        Pixel::BGRA,
        Pixel::RGB24,
    ];
    for format in formats {
        // Native, odd-sized native, and resampling with odd slice/chroma boundaries.
        for (input_size, output_size) in [
            ((1920, 1080), (1920, 1080)),
            ((1921, 1081), (1921, 1081)),
            ((1933, 1091), (1920, 1080)),
            ((161, 91), (79, 43)),
            ((3840, 2160), (1920, 1080)),
        ] {
            let mut frame = patterned_format(format, input_size.0, input_size.1);
            for high_quality in [false, true] {
                let mut original = LegacyScaler::new(&frame, output_size, high_quality);
                let mut threaded = ScalingContext::get_with_forced_threads(
                    format,
                    input_size.0,
                    input_size.1,
                    Pixel::RGBA,
                    output_size.0,
                    output_size.1,
                    scaling_flags(high_quality),
                    2,
                )
                .expect("create production two-thread scaler");
                assert_eq!(threaded.selected_thread_count(), 2);
                for (space, range) in [
                    (Space::BT709, Range::MPEG),
                    (Space::SMPTE170M, Range::JPEG),
                    (Space::BT2020NCL, Range::MPEG),
                    (Space::BT709, Range::MPEG),
                ] {
                    frame.set_color_space(space);
                    frame.set_color_range(range);
                    let expected = original.scale(&frame, output_size, high_quality, &timings);
                    configure_scaler_color(&mut threaded, &frame, format).unwrap();
                    let mut rgba = Video::empty();
                    threaded
                        .run(&frame, &mut rgba)
                        .expect("convert production two-thread frame");
                    let actual = copy_rgba_frame(&rgba, output_size.0, output_size.1).unwrap();
                    let label = format!(
                        "{format:?} {input_size:?}->{output_size:?} HQ={high_quality} {space:?}/{range:?}"
                    );
                    // A compact failure locates the first differing channel instead
                    // of dumping megabytes of frame data into the test log.
                    assert_eq!(actual.len(), expected.len(), "{label}");
                    assert_eq!(
                        actual.iter().zip(&expected).position(|(a, b)| a != b),
                        None,
                        "{label}: first mismatching active channel"
                    );
                }
            }
        }
    }
}
