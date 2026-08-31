use super::*;
use ffmpeg::util::color::{Range, Space};

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
