use super::*;
use std::{
    fs,
    process::{Command, Stdio},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

struct GeneratedFixture {
    path: std::path::PathBuf,
}

impl Drop for GeneratedFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn ffmpeg_available_for_scrub_seek_test() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn generated_fixture_path(label: &str, extension: &str) -> GeneratedFixture {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos();
    GeneratedFixture {
        path: std::env::temp_dir().join(format!(
            "maelstrom-scrub-seek-{label}-{}-{nanos}.{extension}",
            std::process::id()
        )),
    }
}

fn scrub_seek_media(b_frames: bool) -> GeneratedFixture {
    let fixture = generated_fixture_path(if b_frames { "mpeg4-bf2" } else { "mpeg4" }, "mp4");
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=64x48:r=30:d=5",
        "-an",
        "-c:v",
        "mpeg4",
        "-q:v",
        "5",
        "-g",
        "12",
    ]);
    if b_frames {
        command.args(["-bf", "2"]);
    }
    let status = command
        .arg(&fixture.path)
        .status()
        .expect("start scrub seek FFmpeg fixture");
    assert!(status.success(), "FFmpeg did not create scrub seek fixture");
    fixture
}

fn reordered_scrub_seek_media() -> GeneratedFixture {
    let fixture = generated_fixture_path("reordered", "ts");
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=24",
            "-vf",
            "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,4)+eq(n,6)+eq(n,8)+eq(n,11)+eq(n,12)'",
            "-frames:v",
            "8",
            "-fps_mode",
            "vfr",
            "-an",
            "-c:v",
            "mpeg2video",
            "-q:v",
            "8",
            "-g",
            "8",
            "-bf",
            "2",
            "-fflags",
            "+bitexact",
            "-flags:v",
            "+bitexact",
            "-map_metadata",
            "-1",
            "-muxdelay",
            "0",
        ])
        .arg(&fixture.path)
        .status()
        .expect("start reordered scrub seek FFmpeg fixture");
    assert!(
        status.success(),
        "FFmpeg did not create reordered scrub seek fixture"
    );
    fixture
}

fn scrub_request(path: std::path::PathBuf, request_id: u64) -> DecodeRequest {
    DecodeRequest {
        project_epoch: 1,
        cache_epoch: 1,
        request_id,
        media_id: 1,
        path,
        source_tick: 0,
        width: 64,
        height: 48,
        is_scrubbing: false,
        prewarm_scrub_workers: false,
        high_quality_scaling: true,
        progressive_scrub_frames: false,
        source_frame_duration_tick: Some(33_334),
        acceleration: AccelerationPreference::Software,
    }
}

fn decode_scrub_frame(
    monitor: &mut StickyMonitor,
    request: &DecodeRequest,
    timings: &DecoderStageTimingAccumulators,
) -> DecodedRgba {
    monitor
        .decode(
            request,
            || false,
            || None,
            &mut |_| {},
            &mut |_| {},
            timings,
        )
        .unwrap_or_else(|error| {
            panic!(
                "decode scrub seek frame at {} (request {}): {error}",
                request.source_tick, request.request_id
            )
        })
        .expect("scrub seek frame was not superseded")
}

fn decode_scrub_attempt(
    monitor: &mut StickyMonitor,
    request: &DecodeRequest,
    timings: &DecoderStageTimingAccumulators,
) -> MonitorDecodeAttempt {
    monitor
        .decode_attempt(
            request,
            || false,
            || None,
            &mut |_| {},
            &mut |_| {},
            timings,
            false,
        )
        .expect("decode initial scrub seek attempt")
}

fn assert_matches_reference(frame: &DecodedRgba, target: i64, reference: &[(i64, Arc<[u8]>)]) {
    assert!(
        (frame.source_tick - target).abs() <= SOURCE_TIMESTAMP_ROUNDING_TOLERANCE_TICKS,
        "scrub target {target} decoded {}",
        frame.source_tick
    );
    let (_, expected) = reference
        .iter()
        .find(|(source_tick, _)| {
            (*source_tick - target).abs() <= SOURCE_TIMESTAMP_ROUNDING_TOLERANCE_TICKS
        })
        .unwrap_or_else(|| panic!("missing sequential reference frame for {target}"));
    assert_eq!(
        frame.rgba.as_ref(),
        expected.as_ref(),
        "scrub target {target}"
    );
}

fn assert_scrub_seek_matches_sequential_reference(first: DecodeRequest, packet_bound: Option<u64>) {
    let pool = MonitorSessionPool::new(1, 0);
    let mut reference_monitor = open_sticky_monitor(&first, &pool, 0, false)
        .expect("open sequential reference monitor")
        .expect("reference monitor foreground permit");
    let reference_timings = DecoderStageTimingAccumulators::default();
    let mut reference = Vec::with_capacity(150);
    for frame_index in 0..150_i64 {
        let mut request = first.clone();
        request.request_id += frame_index as u64;
        request.source_tick = (frame_index * 1_000_000 + 29) / 30;
        let frame = decode_scrub_frame(&mut reference_monitor, &request, &reference_timings);
        assert!(
            (frame.source_tick - request.source_tick).abs()
                <= SOURCE_TIMESTAMP_ROUNDING_TOLERANCE_TICKS,
            "reference target {} decoded {}",
            request.source_tick,
            frame.source_tick
        );
        reference.push((frame.source_tick, Arc::clone(&frame.rgba)));
    }
    drop(reference_monitor);

    let targets = [
        3_500_000,
        3_600_000,
        1_500_000,
        4_500_000,
        (31_i64 * 1_000_000 + 29) / 30,
    ];
    let mut fresh = first.clone();
    fresh.request_id = 1_000;
    fresh.source_tick = targets[0];
    fresh.is_scrubbing = true;
    let mut fresh_monitor = open_sticky_monitor(&fresh, &pool, 0, false)
        .expect("open fresh scrub monitor")
        .expect("fresh scrub monitor foreground permit");
    let fresh_timings = DecoderStageTimingAccumulators::default();
    let fresh_frame = decode_scrub_frame(&mut fresh_monitor, &fresh, &fresh_timings);
    assert_matches_reference(&fresh_frame, fresh.source_tick, &reference);
    let fresh_packets = fresh_timings.snapshot().demux_packet.samples;
    if let Some(bound) = packet_bound {
        assert!(
            fresh_packets <= bound,
            "fresh scrub seek traversed {fresh_packets} packets"
        );
    } else {
        eprintln!(
            "fresh scrub target {}: {fresh_packets} demux packets",
            fresh.source_tick
        );
    }
    drop(fresh_monitor);

    let mut monitor = open_sticky_monitor(&first, &pool, 0, false)
        .expect("open reusable scrub monitor")
        .expect("reusable scrub monitor foreground permit");
    let timings = DecoderStageTimingAccumulators::default();
    for (index, target) in targets.into_iter().enumerate() {
        let before = timings.snapshot().demux_packet.samples;
        let mut request = first.clone();
        request.request_id = 2_000 + index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        let frame = decode_scrub_frame(&mut monitor, &request, &timings);
        assert_matches_reference(&frame, target, &reference);
        let traversed = timings.snapshot().demux_packet.samples - before;
        if let Some(bound) = packet_bound {
            assert!(
                traversed <= bound,
                "scrub target {target} traversed {traversed} demux packets"
            );
        } else {
            eprintln!("scrub target {target}: {traversed} demux packets");
        }
    }
    drop(monitor);
}

#[test]
fn scrub_seek_uses_nearby_preroll_and_matches_sequential_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let fixture = scrub_seek_media(false);
    assert_scrub_seek_matches_sequential_reference(
        scrub_request(fixture.path.clone(), 1),
        Some(24),
    );
}

#[test]
fn scrub_seek_mpeg4_b_frames_matches_sequential_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let fixture = scrub_seek_media(true);
    assert_scrub_seek_matches_sequential_reference(
        scrub_request(fixture.path.clone(), 2_250),
        None,
    );
}

#[test]
fn supplied_h264_scrub_seek_matches_sequential_reference() {
    // The approved non-GPL bundle has no software H.264 encoder. Supply a five-second,
    // 30 fps fixture made with a supported hardware encoder; decoding stays Software.
    let Some(path) = std::env::var_os("MAELSTROM_SCRUB_H264_TEST_MEDIA") else {
        return;
    };
    assert_scrub_seek_matches_sequential_reference(scrub_request(PathBuf::from(path), 2_500), None);
}

#[test]
fn scrub_seek_reordered_mpeg2_matches_sequential_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let fixture = reordered_scrub_seek_media();
    let boundaries = [
        0, 41_666, 125_000, 166_666, 250_000, 333_333, 458_333, 500_000,
    ];
    let pool = MonitorSessionPool::new(1, 0);
    let first = scrub_request(fixture.path.clone(), 3_000);
    let mut reference_monitor = open_sticky_monitor(&first, &pool, 0, false)
        .expect("open reordered reference monitor")
        .expect("reordered reference monitor foreground permit");
    let reference_timings = DecoderStageTimingAccumulators::default();
    let mut reference = Vec::with_capacity(boundaries.len());
    for (index, target) in boundaries.into_iter().enumerate() {
        let mut request = first.clone();
        request.request_id += index as u64;
        request.source_tick = target;
        let frame = decode_scrub_frame(&mut reference_monitor, &request, &reference_timings);
        assert!(
            (frame.source_tick - target).abs() <= SOURCE_TIMESTAMP_ROUNDING_TOLERANCE_TICKS,
            "reordered reference target {target} decoded {}",
            frame.source_tick
        );
        reference.push((frame.source_tick, Arc::clone(&frame.rgba)));
    }
    drop(reference_monitor);

    let mut initial_attempt = first.clone();
    initial_attempt.request_id = 3_500;
    initial_attempt.source_tick = 250_000;
    initial_attempt.is_scrubbing = true;
    let mut attempt_monitor = open_sticky_monitor(&initial_attempt, &pool, 0, false)
        .expect("open reordered initial-attempt monitor")
        .expect("reordered initial-attempt monitor foreground permit");
    let attempt_timings = DecoderStageTimingAccumulators::default();
    match decode_scrub_attempt(&mut attempt_monitor, &initial_attempt, &attempt_timings) {
        MonitorDecodeAttempt::RetryPreroll => {
            eprintln!("reordered scrub initial seek requested conservative preroll");
        }
        MonitorDecodeAttempt::Frame(_) => {
            panic!("reordered fixture must exercise the conservative-preroll retry");
        }
        MonitorDecodeAttempt::Invalidated => panic!("reordered initial seek was invalidated"),
    }
    drop(attempt_monitor);

    let mut monitor = open_sticky_monitor(&first, &pool, 0, false)
        .expect("open reordered scrub monitor")
        .expect("reordered scrub monitor foreground permit");
    let timings = DecoderStageTimingAccumulators::default();
    for (index, target) in [250_000, 125_000, 458_333].into_iter().enumerate() {
        let before = timings.snapshot().demux_packet.samples;
        let mut request = first.clone();
        request.request_id = 4_000 + index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        let frame = decode_scrub_frame(&mut monitor, &request, &timings);
        assert_matches_reference(&frame, target, &reference);
        let traversed = timings.snapshot().demux_packet.samples - before;
        eprintln!("reordered scrub target {target}: {traversed} demux packets");
        assert!(
            traversed <= 24,
            "reordered scrub target {target} traversed {traversed} demux packets"
        );
    }
}
