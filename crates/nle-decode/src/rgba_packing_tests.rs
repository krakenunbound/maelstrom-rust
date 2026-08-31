use super::*;

fn legacy_pack(frame: &Video, output_size: (u32, u32)) -> Arc<[u8]> {
    let scaled = copy_rgba_frame(frame, frame.width(), frame.height()).unwrap();
    Arc::from(letterbox_rgba(
        scaled,
        (frame.width(), frame.height()),
        output_size,
    ))
}

fn production_pack(frame: &Video, output: (u32, u32)) -> Arc<[u8]> {
    pack_monitor_rgba(frame, (frame.width(), frame.height()), output).unwrap()
}

fn patterned_rgba(width: u32, height: u32) -> Video {
    let mut frame = Video::new(Pixel::RGBA, width, height);
    frame.data_mut(0).fill(0xEE);
    let stride = frame.stride(0);
    for row in 0..height as usize {
        for column in 0..width as usize {
            let offset = row * stride + column * 4;
            frame.data_mut(0)[offset..offset + 4].copy_from_slice(&[
                (column % 256) as u8,
                (row % 256) as u8,
                ((column + row) % 256) as u8,
                ((column ^ row) % 256) as u8,
            ]);
        }
    }
    frame
}

fn percentile_us(values: &mut [u128], percentile: usize) -> f64 {
    values.sort_unstable();
    values[(values.len() * percentile).div_ceil(100) - 1] as f64 / 1000.0
}

#[test]
fn production_packing_matches_legacy_for_native_and_centered_frames() {
    for width in 1..=9 {
        for height in 1..=9 {
            let frame = patterned_rgba(width, height);
            for extra_width in 0..=3 {
                for extra_height in 0..=3 {
                    let output = (width + extra_width, height + extra_height);
                    assert_eq!(production_pack(&frame, output), legacy_pack(&frame, output));
                }
            }
        }
    }
}

#[test]
fn production_packing_preserves_full_resolution_pixels_and_owned_lifetime() {
    for (scaled, output) in [
        ((1920, 1080), (1920, 1080)),
        ((810, 1080), (1920, 1080)),
        ((1920, 810), (1920, 1080)),
        ((1919, 1079), (1920, 1080)),
        ((3840, 2160), (3840, 2160)),
    ] {
        let mut frame = patterned_rgba(scaled.0, scaled.1);
        let expected = legacy_pack(&frame, output);
        let actual = production_pack(&frame, output);
        frame.data_mut(0).fill(0xAA);
        drop(frame);
        assert_eq!(actual.len(), expected.len());
        assert_eq!(
            actual.iter().zip(expected.iter()).position(|(a, b)| a != b),
            None
        );
    }
}

#[test]
fn rgba_adapter_rejects_invalid_scaler_output_before_accessing_pixels() {
    assert!(pack_monitor_rgba(&Video::empty(), (1, 1), (1, 1)).is_err());
    let bgra = Video::new(Pixel::BGRA, 4, 2);
    assert!(pack_monitor_rgba(&bgra, (4, 2), (4, 2)).is_err());
    let mut rgba = patterned_rgba(4, 2);
    assert!(pack_monitor_rgba(&rgba, (3, 2), (4, 2)).is_err());
    assert!(pack_monitor_rgba(&rgba, (4, 2), (3, 2)).is_err());
    // SAFETY: the owned frame remains alive; only linesize metadata is altered.
    // Its AVBufferRef allocation is untouched and owns destruction independently.
    unsafe {
        (*rgba.as_mut_ptr()).linesize[0] = 1_000_000;
    }
    assert!(pack_monitor_rgba(&rgba, (4, 2), (4, 2)).is_err());
    unsafe {
        (*rgba.as_mut_ptr()).linesize[0] = -32;
    }
    assert!(pack_monitor_rgba(&rgba, (4, 2), (4, 2)).is_err());
    unsafe {
        (*rgba.as_mut_ptr()).linesize[0] = 0;
    }
    assert!(pack_monitor_rgba(&rgba, (4, 2), (4, 2)).is_err());
}

#[test]
fn monitor_frame_pack_keeps_identity_and_records_complete_packing_span() {
    ffmpeg::init().unwrap();
    let decoded = patterned_rgba(4, 2);
    let timings = DecoderStageTimingAccumulators::default();
    let mut scaler = Some(
        ScalingContext::get(Pixel::RGBA, 4, 2, Pixel::RGBA, 4, 2, scaling_flags(true)).unwrap(),
    );
    let packed = pack_decoded_monitor_frame(
        &mut scaler,
        &mut Some((Pixel::RGBA, 4, 2)),
        &mut Some(true),
        &mut (4, 2),
        &decoded,
        false,
        (4, 4),
        true,
        7,
        83_334,
        83_333,
        &timings,
    )
    .unwrap();
    assert_eq!(
        (packed.request_id, packed.target_tick, packed.source_tick),
        (7, 83_334, 83_333)
    );
    assert_eq!((packed.width, packed.height), (4, 4));
    assert_eq!(packed.rgba, legacy_pack(&decoded, (4, 4)));
    let measured = timings.snapshot();
    assert_eq!(measured.rgba_copy_letterbox.samples, 1);
    assert_eq!(measured.scaler.samples, 1);
    assert_eq!(measured.hardware_transfer.samples, 0);
}

#[test]
#[ignore = "full-resolution RGBA packing allocation/timing diagnostic"]
fn full_resolution_rgba_packing_probe() {
    for (scaled, output) in [
        ((1920, 1080), (1920, 1080)),
        ((810, 1080), (1920, 1080)),
        ((1920, 810), (1920, 1080)),
        ((1919, 1079), (1920, 1080)),
        ((3840, 2160), (3840, 2160)),
    ] {
        let frame = patterned_rgba(scaled.0, scaled.1);
        let expected = legacy_pack(&frame, output);
        for (name, pack) in [
            ("legacy", legacy_pack as fn(&Video, (u32, u32)) -> Arc<[u8]>),
            ("production", production_pack),
        ] {
            for _ in 0..8 {
                std::hint::black_box(pack(&frame, output));
            }
            let mut samples = Vec::with_capacity(120);
            for _ in 0..120 {
                let started = Instant::now();
                let actual = std::hint::black_box(pack(&frame, output));
                samples.push(started.elapsed().as_nanos());
                assert_eq!(actual.len(), expected.len());
                assert_eq!(
                    actual.iter().zip(expected.iter()).position(|(a, b)| a != b),
                    None,
                    "{name} {scaled:?}->{output:?} first differing RGBA byte"
                );
            }
            let p50 = percentile_us(&mut samples, 50);
            let p95 = percentile_us(&mut samples, 95);
            eprintln!("packing {name} {scaled:?}->{output:?}: p50={p50:.3}us p95={p95:.3}us");
        }
    }
}
