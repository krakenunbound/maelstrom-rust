#[cfg(target_os = "windows")]
use super::tests::{hardware_test_guard, open_supplied_media_windows_hardware_monitor};
use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
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

fn real_codec_vfr_media(label: &str, codec_arguments: &[&str]) -> GeneratedFixture {
    let fixture = generated_fixture_path(label, "mov");
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=24",
        "-vf",
        "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,4)+eq(n,6)+eq(n,8)+eq(n,11)+eq(n,12)',setpts=PTS+7/TB",
        "-frames:v",
        "8",
        "-fps_mode",
        "vfr",
        "-an",
    ]);
    command.args(codec_arguments);
    let status = command
        .args([
            "-fflags",
            "+bitexact",
            "-flags:v",
            "+bitexact",
            "-map_metadata",
            "-1",
        ])
        .arg(&fixture.path)
        .status()
        .expect("start real-codec VFR FFmpeg fixture");
    assert!(
        status.success(),
        "FFmpeg did not create {label} real-codec VFR fixture"
    );
    fixture
}

struct CliSequentialReference {
    stream_origin: i64,
    timestamps: Vec<i64>,
    rgba: Vec<Arc<[u8]>>,
}

fn ffprobe_stream_properties(path: &Path) -> Vec<String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt,profile,nb_frames",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .expect("start FFprobe stream inspection");
    assert!(
        output.status.success(),
        "FFprobe could not inspect {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("FFprobe stream properties were UTF-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn ffprobe_frame_timestamps(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#32",
            "-show_frames",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("start FFprobe frame timestamp inspection");
    assert!(
        output.status.success(),
        "FFprobe could not inspect {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    // CSV can append side-data fields. Only the leading frame timestamp belongs to this query.
    String::from_utf8(output.stdout)
        .expect("FFprobe frame timestamps were UTF-8")
        .lines()
        .filter_map(|line| {
            match line
                .split_once(',')
                .map_or(line, |(timestamp, _)| timestamp)
            {
                "" => None,
                timestamp => Some(
                    parse_timestamp_microseconds(timestamp)
                        .unwrap_or_else(|| panic!("invalid FFprobe timestamp {timestamp:?}")),
                ),
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn ffprobe_packet_timestamps(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#32",
            "-show_packets",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("start FFprobe packet timestamp inspection");
    assert!(
        output.status.success(),
        "FFprobe could not inspect packet timestamps for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("FFprobe packet timestamps were UTF-8")
        .lines()
        .map(|timestamp| {
            parse_timestamp_microseconds(timestamp)
                .unwrap_or_else(|| panic!("invalid FFprobe packet timestamp {timestamp:?}"))
        })
        .collect()
}

fn parse_timestamp_microseconds(timestamp: &str) -> Option<i64> {
    let (negative, decimal) = match timestamp.strip_prefix('-') {
        Some(decimal) => (true, decimal),
        None => (false, timestamp),
    };
    let (whole, fraction) = decimal.split_once('.').unwrap_or((decimal, ""));
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole = whole.parse::<i128>().ok()?;
    let mut microseconds = whole.checked_mul(1_000_000)?;
    let mut digits = fraction.bytes();
    let mut fractional = 0_i128;
    for _ in 0..6 {
        fractional *= 10;
        if let Some(digit) = digits.next() {
            fractional += i128::from(digit - b'0');
        }
    }
    if digits.next().is_some_and(|digit| digit >= b'5') {
        fractional += 1;
    }
    microseconds = microseconds.checked_add(fractional)?;
    if negative {
        microseconds = -microseconds;
    }
    i64::try_from(microseconds).ok()
}

fn cli_sequential_reference_with_filter(
    path: &Path,
    frame_count: usize,
    canvas: (u32, u32),
    filter: &str,
) -> CliSequentialReference {
    let raw_timestamps = ffprobe_frame_timestamps(path);
    assert_eq!(
        raw_timestamps.len(),
        frame_count,
        "FFprobe frame count for {}",
        path.display()
    );
    let stream_origin = *raw_timestamps
        .first()
        .expect("nonempty FFprobe timestamp list");
    let timestamps = raw_timestamps
        .into_iter()
        .map(|timestamp| timestamp - stream_origin)
        .collect();
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-hwaccel", "none", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            filter,
            "-frames:v",
            &frame_count.to_string(),
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .expect("start independent sequential FFmpeg reference decode");
    assert!(
        output.status.success(),
        "FFmpeg could not create sequential reference for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes_per_frame = canvas.0 as usize * canvas.1 as usize * 4;
    assert_eq!(
        output.stdout.len(),
        frame_count * bytes_per_frame,
        "independent reference byte count for {}",
        path.display()
    );
    let rgba = output
        .stdout
        .chunks_exact(bytes_per_frame)
        .map(Arc::<[u8]>::from)
        .collect();
    CliSequentialReference {
        stream_origin,
        timestamps,
        rgba,
    }
}

#[cfg(target_os = "windows")]
fn cli_av1_hardware_reference_with_filter(
    path: &Path,
    frame_count: usize,
    canvas: (u32, u32),
    filter: &str,
    requested_backend: DecodeBackend,
) -> CliSequentialReference {
    // AV1 frame best-effort timestamps are unavailable with this fixture's default
    // decoder. Packet PTS are the source timestamps used by the AV1 decoder path.
    let raw_timestamps = ffprobe_packet_timestamps(path);
    assert_eq!(
        raw_timestamps.len(),
        frame_count,
        "FFprobe AV1 packet count for {}",
        path.display()
    );
    let stream_origin = *raw_timestamps
        .first()
        .expect("nonempty FFprobe AV1 packet timestamp list");
    let timestamps = raw_timestamps
        .into_iter()
        .map(|timestamp| timestamp - stream_origin)
        .collect();
    let reference_decoder = if requested_backend == DecodeBackend::Nvidia {
        "av1_qsv"
    } else {
        "av1_cuvid"
    };
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-c:v", reference_decoder, "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            filter,
            "-frames:v",
            &frame_count.to_string(),
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .expect("start independent AV1 named-hardware FFmpeg reference decode");
    assert!(
        output.status.success(),
        "FFmpeg {reference_decoder} could not create AV1 reference for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes_per_frame = canvas.0 as usize * canvas.1 as usize * 4;
    assert_eq!(
        output.stdout.len(),
        frame_count * bytes_per_frame,
        "independent AV1 reference byte count for {}",
        path.display()
    );
    let rgba = output
        .stdout
        .chunks_exact(bytes_per_frame)
        .map(Arc::<[u8]>::from)
        .collect();
    CliSequentialReference {
        stream_origin,
        timestamps,
        rgba,
    }
}

fn cli_sequential_reference(path: &Path, frame_count: usize) -> CliSequentialReference {
    cli_sequential_reference_with_filter(
        path,
        frame_count,
        (64, 48),
        "scale=64:36:flags=bicubic+accurate_rnd,format=rgba,pad=64:48:0:6:color=black@0",
    )
}

#[cfg(target_os = "windows")]
fn ffprobe_video_dimensions(path: &Path) -> (u32, u32) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("start FFprobe video dimensions inspection");
    assert!(
        output.status.success(),
        "FFprobe could not inspect dimensions for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let dimensions_text = String::from_utf8(output.stdout).expect("FFprobe dimensions were UTF-8");
    let dimensions = dimensions_text
        .trim()
        .split_once(',')
        .unwrap_or_else(|| panic!("FFprobe did not return width,height for {}", path.display()));
    (
        dimensions
            .0
            .parse()
            .unwrap_or_else(|_| panic!("invalid FFprobe width {:?}", dimensions.0)),
        dimensions
            .1
            .parse()
            .unwrap_or_else(|_| panic!("invalid FFprobe height {:?}", dimensions.1)),
    )
}

fn assert_real_codec_frame(
    frame: &DecodedRgba,
    request: &DecodeRequest,
    reference: &CliSequentialReference,
    label: &str,
) {
    assert_eq!(
        frame.request_id, request.request_id,
        "decoded request ownership"
    );
    assert_eq!(
        frame.target_tick, request.source_tick,
        "decoded target ownership"
    );
    assert!(
        (frame.source_tick - request.source_tick).abs()
            <= SOURCE_TIMESTAMP_ROUNDING_TOLERANCE_TICKS,
        "scrub target {} decoded {}",
        request.source_tick,
        frame.source_tick
    );
    let index = reference
        .timestamps
        .iter()
        .position(|timestamp| {
            (*timestamp - request.source_tick).abs() <= SOURCE_TIMESTAMP_ROUNDING_TOLERANCE_TICKS
        })
        .unwrap_or_else(|| panic!("missing CLI reference frame for {}", request.source_tick));
    let expected = reference.rgba[index].as_ref();
    let actual = frame.rgba.as_ref();
    assert_eq!(
        actual.len(),
        expected.len(),
        "{label} scrub target {} RGBA byte count",
        request.source_tick
    );
    if actual != expected {
        let first = actual
            .iter()
            .zip(expected)
            .position(|(actual, expected)| actual != expected)
            .expect("unequal RGBA buffers have a differing byte");
        let differences = actual
            .iter()
            .zip(expected)
            .filter(|(actual, expected)| actual != expected)
            .count();
        panic!(
            "{label} scrub target {} differs from independent CLI reference: {differences} / {} RGBA bytes, first byte {first} (actual {}, reference {})",
            request.source_tick,
            actual.len(),
            actual[first],
            expected[first],
        );
    }
}

fn assert_real_codec_vfr_seek_matches_cli_reference(
    label: &str,
    path: PathBuf,
    expected_properties: &[&str],
    request_id: u64,
) {
    let properties = ffprobe_stream_properties(&path);
    assert!(
        properties.iter().any(|value| value == "8"),
        "{label} must declare eight frames"
    );
    for property in expected_properties {
        assert!(
            properties.iter().any(|value| value == property),
            "{label} expected FFprobe property {property:?}, got {properties:?}"
        );
    }
    let reference = cli_sequential_reference(&path, 8);
    assert!(
        reference.stream_origin > 0,
        "{label} stream origin must be positive"
    );
    let gaps: std::collections::BTreeSet<_> = reference
        .timestamps
        .windows(2)
        .map(|timestamps| timestamps[1] - timestamps[0])
        .collect();
    assert!(
        gaps.len() > 1 && gaps.iter().all(|gap| *gap > 0),
        "{label} must retain distinct positive VFR timestamp gaps: {gaps:?}"
    );

    let mut first = scrub_request(path.clone(), request_id);
    first.source_tick = reference.timestamps[0];
    first.source_frame_duration_tick = None;
    let pool = MonitorSessionPool::new(1, 0);
    let mut cases = 0;

    let mut monitor = open_sticky_monitor(&first, &pool, 0, false)
        .expect("open real-codec reusable monitor")
        .expect("real-codec reusable monitor foreground permit");
    assert_eq!(monitor.backend, DecodeBackend::Software, "{label} backend");
    let timings = DecoderStageTimingAccumulators::default();
    for (index, target) in reference.timestamps.iter().copied().enumerate() {
        let mut request = first.clone();
        request.request_id += index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        let frame = decode_scrub_frame(&mut monitor, &request, &timings);
        assert_real_codec_frame(&frame, &request, &reference, label);
        cases += 1;
    }
    for (index, target) in reference.timestamps.iter().copied().rev().enumerate() {
        let mut request = first.clone();
        request.request_id += 100 + index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        let frame = decode_scrub_frame(&mut monitor, &request, &timings);
        assert_real_codec_frame(&frame, &request, &reference, label);
        cases += 1;
    }
    let mut final_request = first.clone();
    final_request.request_id += 200;
    final_request.source_tick = *reference
        .timestamps
        .last()
        .expect("eight reference timestamps");
    final_request.is_scrubbing = true;
    let frame = decode_scrub_frame(&mut monitor, &final_request, &timings);
    assert_real_codec_frame(&frame, &final_request, &reference, label);
    cases += 1;
    drop(monitor);

    for (index, target) in [reference.timestamps[3], reference.timestamps[6]]
        .into_iter()
        .enumerate()
    {
        let mut request = first.clone();
        request.request_id += 300 + index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        let mut fresh_monitor = open_sticky_monitor(&request, &pool, 0, false)
            .expect("open real-codec fresh monitor")
            .expect("real-codec fresh monitor foreground permit");
        assert_eq!(
            fresh_monitor.backend,
            DecodeBackend::Software,
            "{label} fresh backend"
        );
        let fresh_timings = DecoderStageTimingAccumulators::default();
        let frame = decode_scrub_frame(&mut fresh_monitor, &request, &fresh_timings);
        assert_real_codec_frame(&frame, &request, &reference, label);
        cases += 1;
    }
    eprintln!(
        "{label}: {} VFR boundaries, {cases} exact CLI-reference seek cases",
        reference.timestamps.len()
    );
}

#[cfg(target_os = "windows")]
fn assert_windows_hardware_vfr_output_matches_cli_reference(
    label: &str,
    path: PathBuf,
    requested_backend: DecodeBackend,
    request_id: u64,
    output_size: (u32, u32),
    reference: &CliSequentialReference,
) -> usize {
    let mut first = scrub_request(path.clone(), request_id);
    first.source_tick = reference.timestamps[0];
    first.source_frame_duration_tick = None;
    first.acceleration = AccelerationPreference::PreferHardware;
    first.width = output_size.0;
    first.height = output_size.1;
    let decode_and_check = |monitor: &mut StickyMonitor,
                            request: &DecodeRequest,
                            timings: &DecoderStageTimingAccumulators| {
        let previous_transfers = timings.snapshot().hardware_transfer.samples;
        let frame = decode_scrub_frame(monitor, request, timings);
        assert_real_codec_frame(&frame, request, reference, label);
        assert_eq!(
            (frame.width, frame.height),
            output_size,
            "{label} dimensions"
        );
        assert_eq!(monitor.backend, requested_backend, "{label} actual backend");
        assert_eq!(monitor.fallback_reason, None, "{label} fallback reason");
        if requires_cpu_frame_transfer(requested_backend) {
            assert!(
                timings.snapshot().hardware_transfer.samples > previous_transfers,
                "{label} did not transfer a hardware frame"
            );
        }
    };
    let mut cases = 0;
    let timings = DecoderStageTimingAccumulators::default();
    let mut monitor =
        open_supplied_media_windows_hardware_monitor(path.clone(), requested_backend, output_size)
            .unwrap_or_else(|error| {
                panic!(
                    "{label}: could not open {}: {error}",
                    requested_backend.display_name()
                )
            });
    assert_eq!(monitor.backend, requested_backend, "{label} actual backend");
    assert_eq!(monitor.fallback_reason, None, "{label} fallback reason");
    if requires_cpu_frame_transfer(requested_backend) {
        assert!(
            monitor.transfer_hardware_frames,
            "{label} hardware frame transfer"
        );
    }

    // Forward, reverse (including repeated final), then return to the final frame.
    let targets = reference
        .timestamps
        .iter()
        .chain(reference.timestamps.iter().rev())
        .chain(reference.timestamps.last())
        .copied();
    for (index, target) in targets.enumerate() {
        let mut request = first.clone();
        request.request_id += index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        decode_and_check(&mut monitor, &request, &timings);
        cases += 1;
    }
    drop(monitor);

    for (index, target) in [reference.timestamps[3], reference.timestamps[6]]
        .into_iter()
        .enumerate()
    {
        let mut request = first.clone();
        request.request_id += 300 + index as u64;
        request.source_tick = target;
        request.is_scrubbing = true;
        let mut fresh_monitor = open_supplied_media_windows_hardware_monitor(
            path.clone(),
            requested_backend,
            output_size,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{label}: could not open fresh {}: {error}",
                requested_backend.display_name()
            )
        });
        assert_eq!(
            fresh_monitor.backend, requested_backend,
            "{label} fresh actual backend"
        );
        assert_eq!(
            fresh_monitor.fallback_reason, None,
            "{label} fresh fallback reason"
        );
        let fresh_timings = DecoderStageTimingAccumulators::default();
        decode_and_check(&mut fresh_monitor, &request, &fresh_timings);
        assert_eq!(
            fresh_timings.snapshot().named_decoder_reopen.samples,
            0,
            "{label} fresh monitor named decoder reopen samples"
        );
        cases += 1;
    }
    if requires_cpu_frame_transfer(requested_backend) {
        assert!(
            timings.snapshot().hardware_transfer.samples > 0,
            "{label} reusable monitor did not transfer a hardware frame"
        );
    }
    let named_decoder_reopen = timings.snapshot().named_decoder_reopen;
    if requested_backend == DecodeBackend::IntelQuickSync {
        assert_eq!(
            named_decoder_reopen.samples, 7,
            "{label} reusable QSV monitor named decoder reopen samples"
        );
        eprintln!(
            "{label} {} {}x{}: named decoder reopen: {} samples, total {:.3} ms, mean {:.3} ms, max {:.3} ms",
            requested_backend.display_name(),
            output_size.0,
            output_size.1,
            named_decoder_reopen.samples,
            named_decoder_reopen.total_ms(),
            named_decoder_reopen.mean_ms(),
            named_decoder_reopen.max_ms(),
        );
    } else {
        assert_eq!(
            named_decoder_reopen.samples, 0,
            "{label} non-QSV reusable monitor named decoder reopen samples"
        );
    }
    assert_eq!(cases, 19, "{label} hardware parity case count");
    eprintln!(
        "{label} {} {}x{}: {} VFR boundaries, {cases} exact CLI-reference seek cases",
        requested_backend.display_name(),
        output_size.0,
        output_size.1,
        reference.timestamps.len()
    );
    cases
}

#[cfg(target_os = "windows")]
fn assert_windows_hardware_vfr_seek_matches_cli_reference(
    label: &str,
    path: PathBuf,
    expected_codec: &str,
    requested_backend: DecodeBackend,
    request_id: u64,
) {
    let properties = ffprobe_stream_properties(&path);
    assert!(
        properties.iter().any(|value| value == "8"),
        "{label} must declare eight frames"
    );
    assert!(
        properties.iter().any(|value| value == expected_codec),
        "{label} expected FFprobe codec {expected_codec:?}, got {properties:?}"
    );
    if expected_codec == "hevc" {
        assert!(
            properties.iter().any(|value| value == "Main 10"),
            "{label} expected HEVC Main 10, got {properties:?}"
        );
    }
    let native_size = ffprobe_video_dimensions(&path);
    assert!(
        native_size.0 > 0
            && native_size.1 > 0
            && native_size.0 <= 1_920
            && native_size.1 <= 1_080
            && u64::from(native_size.0) * 9 == u64::from(native_size.1) * 16,
        "{label} must be a positive 16:9 source no larger than 1920x1080, got {}x{}",
        native_size.0,
        native_size.1
    );
    let reference = cli_sequential_reference(&path, 8);
    assert!(
        reference.stream_origin > 0,
        "{label} stream origin must be positive"
    );
    let gaps: std::collections::BTreeSet<_> = reference
        .timestamps
        .windows(2)
        .map(|timestamps| timestamps[1] - timestamps[0])
        .collect();
    assert!(
        gaps.len() > 1 && gaps.iter().all(|gap| *gap > 0),
        "{label} must retain distinct positive VFR timestamp gaps: {gaps:?}"
    );
    let scaled_cases = assert_windows_hardware_vfr_output_matches_cli_reference(
        label,
        path.clone(),
        requested_backend,
        request_id,
        (64, 48),
        &reference,
    );
    let native_filter = "scale=iw:ih:flags=bicubic+accurate_rnd,format=rgba";
    let native_reference =
        cli_sequential_reference_with_filter(&path, 8, native_size, native_filter);
    let native_cases = assert_windows_hardware_vfr_output_matches_cli_reference(
        label,
        path,
        requested_backend,
        request_id + 1_000,
        native_size,
        &native_reference,
    );
    assert_eq!(
        scaled_cases + native_cases,
        38,
        "{label} hardware parity case count"
    );
}

#[cfg(target_os = "windows")]
fn assert_windows_hardware_av1_vfr_seek_matches_cli_reference(
    label: &str,
    path: PathBuf,
    requested_backend: DecodeBackend,
    request_id: u64,
) {
    let properties = ffprobe_stream_properties(&path);
    for property in ["av1", "Main", "yuv420p"] {
        assert!(
            properties.iter().any(|value| value == property),
            "{label} expected FFprobe property {property:?}, got {properties:?}"
        );
    }
    assert_eq!(
        ffprobe_video_dimensions(&path),
        (352, 288),
        "{label} AV1 fixture dimensions"
    );
    let reference = cli_av1_hardware_reference_with_filter(
        &path,
        8,
        (64, 48),
        "scale=59:48:flags=bicubic+accurate_rnd,format=rgba,pad=64:48:2:0:color=black@0",
        requested_backend,
    );
    assert!(
        reference.stream_origin > 0,
        "{label} stream origin must be positive"
    );
    let gaps: std::collections::BTreeSet<_> = reference
        .timestamps
        .windows(2)
        .map(|timestamps| timestamps[1] - timestamps[0])
        .collect();
    assert!(
        gaps.len() > 1 && gaps.iter().all(|gap| *gap > 0),
        "{label} must retain distinct positive AV1 VFR timestamp gaps: {gaps:?}"
    );
    let scaled_cases = assert_windows_hardware_vfr_output_matches_cli_reference(
        label,
        path.clone(),
        requested_backend,
        request_id,
        (64, 48),
        &reference,
    );
    // Keep the hardware qualification's large output gate at 1920x1080 even
    // though this deliberately small AOM AV1 source is 352x288.
    let large_reference = cli_av1_hardware_reference_with_filter(
        &path,
        8,
        (1_920, 1_080),
        "scale=1320:1080:flags=bicubic+accurate_rnd,format=rgba,pad=1920:1080:300:0:color=black@0",
        requested_backend,
    );
    let large_cases = assert_windows_hardware_vfr_output_matches_cli_reference(
        label,
        path,
        requested_backend,
        request_id + 1_000,
        (1_920, 1_080),
        &large_reference,
    );
    assert_eq!(
        scaled_cases + large_cases,
        38,
        "{label} hardware AV1 parity case count"
    );
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
        scaling_quality: ScalingQuality::Bicubic,
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

#[test]
fn scrub_seek_real_codec_vfr_generated_prores_matches_independent_cli_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let prores = real_codec_vfr_media(
        "prores-422-10bit-vfr",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "2",
            "-pix_fmt",
            "yuv422p10le",
        ],
    );
    assert_real_codec_vfr_seek_matches_cli_reference(
        "ProRes Standard 10-bit 4:2:2",
        prores.path.clone(),
        &["prores", "Standard", "yuv422p10le"],
        5_000,
    );
}

#[test]
fn scrub_seek_real_codec_vfr_generated_dnxhr_matches_independent_cli_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let dnxhr = real_codec_vfr_media(
        "dnxhr-hqx-10bit-vfr",
        &[
            "-c:v",
            "dnxhd",
            "-profile:v",
            "dnxhr_hqx",
            "-pix_fmt",
            "yuv422p10le",
        ],
    );
    assert_real_codec_vfr_seek_matches_cli_reference(
        "DNxHR HQX 10-bit 4:2:2",
        dnxhr.path.clone(),
        &["dnxhd", "DNXHR HQX", "yuv422p10le"],
        6_000,
    );
}

#[test]
fn scrub_seek_real_codec_vfr_generated_shifted_reordered_mpeg4_matches_cli_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let Some(path) = std::env::var_os("MAELSTROM_SHIFTED_REORDERED_VFR_TEST_MEDIA") else {
        return;
    };
    assert_real_codec_vfr_seek_matches_cli_reference(
        "generated shifted/reordered MPEG-4 VFR",
        PathBuf::from(path),
        &["mpeg4", "Advanced Simple Profile", "yuv420p"],
        7_000,
    );
}

#[test]
fn scrub_seek_real_codec_vfr_hevc_main10_matches_independent_cli_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    let Some(path) = std::env::var_os("MAELSTROM_HEVC_VFR_TEST_MEDIA") else {
        return;
    };
    assert_real_codec_vfr_seek_matches_cli_reference(
        "supplied HEVC Main 10 VFR",
        PathBuf::from(path),
        &["hevc", "Main 10", "yuv420p10le"],
        8_000,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR H.264 fixture"]
fn supplied_windows_d3d11va_h264_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA must name the H.264 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "D3D11VA supplied H.264 VFR",
        path,
        "h264",
        DecodeBackend::D3D11VA,
        8_000,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR H.264 fixture"]
fn supplied_windows_dxva2_h264_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA must name the H.264 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "DXVA2 supplied H.264 VFR",
        path,
        "h264",
        DecodeBackend::DXVA2,
        8_100,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR H.264 fixture"]
fn supplied_windows_cuvid_h264_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA must name the H.264 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "CUVID supplied H.264 VFR",
        path,
        "h264",
        DecodeBackend::Nvidia,
        8_400,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR H.264 fixture"]
fn supplied_windows_qsv_h264_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA must name the H.264 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "QSV supplied H.264 VFR",
        path,
        "h264",
        DecodeBackend::IntelQuickSync,
        8_500,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_AV1_VFR_TEST_MEDIA to point to the supplied 8-frame shifted AOM AV1 fixture"]
fn supplied_windows_d3d11va_av1_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_AV1_VFR_TEST_MEDIA")
            .expect("MAELSTROM_AV1_VFR_TEST_MEDIA must name the AV1 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_av1_vfr_seek_matches_cli_reference(
        "D3D11VA supplied AV1 VFR",
        path,
        DecodeBackend::D3D11VA,
        8_800,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_AV1_VFR_TEST_MEDIA to point to the supplied 8-frame shifted AOM AV1 fixture"]
fn supplied_windows_dxva2_av1_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_AV1_VFR_TEST_MEDIA")
            .expect("MAELSTROM_AV1_VFR_TEST_MEDIA must name the AV1 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_av1_vfr_seek_matches_cli_reference(
        "DXVA2 supplied AV1 VFR",
        path,
        DecodeBackend::DXVA2,
        8_900,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_AV1_VFR_TEST_MEDIA to point to the supplied 8-frame shifted AOM AV1 fixture"]
fn supplied_windows_cuvid_av1_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_AV1_VFR_TEST_MEDIA")
            .expect("MAELSTROM_AV1_VFR_TEST_MEDIA must name the AV1 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_av1_vfr_seek_matches_cli_reference(
        "CUVID supplied AV1 VFR",
        path,
        DecodeBackend::Nvidia,
        9_000,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_AV1_VFR_TEST_MEDIA to point to the supplied 8-frame shifted AOM AV1 fixture"]
fn supplied_windows_qsv_av1_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_AV1_VFR_TEST_MEDIA")
            .expect("MAELSTROM_AV1_VFR_TEST_MEDIA must name the AV1 VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_av1_vfr_seek_matches_cli_reference(
        "QSV supplied AV1 VFR",
        path,
        DecodeBackend::IntelQuickSync,
        9_100,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HEVC_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR HEVC fixture"]
fn supplied_windows_d3d11va_hevc_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HEVC_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HEVC_VFR_TEST_MEDIA must name the HEVC VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "D3D11VA supplied HEVC VFR",
        path,
        "hevc",
        DecodeBackend::D3D11VA,
        8_200,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HEVC_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR HEVC fixture"]
fn supplied_windows_dxva2_hevc_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HEVC_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HEVC_VFR_TEST_MEDIA must name the HEVC VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "DXVA2 supplied HEVC VFR",
        path,
        "hevc",
        DecodeBackend::DXVA2,
        8_300,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HEVC_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR HEVC fixture"]
fn supplied_windows_cuvid_hevc_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HEVC_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HEVC_VFR_TEST_MEDIA must name the HEVC VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "CUVID supplied HEVC VFR",
        path,
        "hevc",
        DecodeBackend::Nvidia,
        8_600,
    );
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires MAELSTROM_HEVC_VFR_TEST_MEDIA to point to the supplied 8-frame shifted VFR HEVC fixture"]
fn supplied_windows_qsv_hevc_vfr_scrub_matches_independent_cli_reference() {
    let path = PathBuf::from(
        std::env::var_os("MAELSTROM_HEVC_VFR_TEST_MEDIA")
            .expect("MAELSTROM_HEVC_VFR_TEST_MEDIA must name the HEVC VFR fixture"),
    );
    let _hardware = hardware_test_guard();
    assert_windows_hardware_vfr_seek_matches_cli_reference(
        "QSV supplied HEVC VFR",
        path,
        "hevc",
        DecodeBackend::IntelQuickSync,
        8_700,
    );
}

#[test]
fn scaler_color_metadata_changes_match_independent_cli_reference() {
    if !ffmpeg_available_for_scrub_seek_test() {
        return;
    }
    use ffmpeg::util::color::{Range, Space};
    let fixture = generated_fixture_path("color-metadata", "yuv");
    let raw: Vec<u8> = [100, 150, 200]
        .into_iter()
        .flat_map(|value| std::iter::repeat_n(value, 64))
        .collect();
    fs::write(&fixture.path, &raw).unwrap();
    let mut decoded = Video::new(Pixel::YUV444P, 8, 8);
    for (plane, value) in [100, 150, 200].into_iter().enumerate() {
        decoded.data_mut(plane).fill(value);
    }
    let (context, size) =
        StickyMonitor::make_scaler(Pixel::YUV444P, 8, 8, 8, 8, ScalingQuality::Bicubic).unwrap();
    let mut scaler = Some(context);
    let mut scaler_input = Some((Pixel::YUV444P, 8, 8));
    let mut quality = Some(ScalingQuality::Bicubic);
    let mut scaled_size = size;
    let timings = DecoderStageTimingAccumulators::default();
    let mut results = Vec::new();
    // One retained scaler must follow per-frame metadata and reset unspecified input
    // to its original defaults, rather than inheriting the previous frame's settings.
    for (space, range, cli_space, cli_range) in [
        (Space::BT709, Range::MPEG, "bt709", "tv"),
        (Space::SMPTE170M, Range::MPEG, "smpte170m", "tv"),
        (Space::BT709, Range::JPEG, "bt709", "pc"),
        (Space::Unspecified, Range::Unspecified, "unknown", "unknown"),
        (Space::BT709, Range::MPEG, "bt709", "tv"),
    ] {
        decoded.set_color_space(space);
        decoded.set_color_range(range);
        let converted = scale_monitor_frame(
            &mut scaler,
            &mut scaler_input,
            &mut quality,
            &mut scaled_size,
            &decoded,
            false,
            (8, 8),
            ScalingQuality::Bicubic,
            &timings,
        )
        .unwrap();
        let actual = copy_rgba_frame(&converted, 8, 8).unwrap();
        let reference = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "rawvideo",
                "-pixel_format",
                "yuv444p",
                "-video_size",
                "8x8",
                "-framerate",
                "1",
                "-colorspace",
                cli_space,
                "-color_range",
                cli_range,
                "-i",
            ])
            .arg(&fixture.path)
            .args([
                "-vf",
                "scale=8:8:flags=bicubic+accurate_rnd,format=rgba",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            reference.status.success(),
            "{}",
            String::from_utf8_lossy(&reference.stderr)
        );
        assert_eq!(reference.stdout.len(), 8 * 8 * 4);
        assert_eq!(
            actual, reference.stdout,
            "matrix {space:?}, range {range:?}"
        );
        results.push(actual);
    }
    assert_ne!(
        results[0], results[1],
        "matrix change must affect these pixels"
    );
    assert_ne!(
        results[0], results[2],
        "range change must affect these pixels"
    );
    assert_eq!(
        results[1], results[3],
        "untagged input retains default BT.601 limited range"
    );
    assert_eq!(
        results[0], results[4],
        "retained scaler returns to the original matrix/range"
    );

    // Deprecated full-range pixel formats retain their intrinsic range even when
    // the AVFrame range property is unspecified.
    let mut jpeg = Video::new(Pixel::YUVJ444P, 8, 8);
    for (plane, value) in [100, 150, 200].into_iter().enumerate() {
        jpeg.data_mut(plane).fill(value);
    }
    jpeg.set_color_space(Space::BT709);
    jpeg.set_color_range(Range::Unspecified);
    let (context, size) =
        StickyMonitor::make_scaler(Pixel::YUVJ444P, 8, 8, 8, 8, ScalingQuality::Bicubic).unwrap();
    scaler = Some(context);
    scaler_input = Some((Pixel::YUVJ444P, 8, 8));
    scaled_size = size;
    let converted = scale_monitor_frame(
        &mut scaler,
        &mut scaler_input,
        &mut quality,
        &mut scaled_size,
        &jpeg,
        false,
        (8, 8),
        ScalingQuality::Bicubic,
        &timings,
    )
    .unwrap();
    assert_eq!(copy_rgba_frame(&converted, 8, 8).unwrap(), results[2]);
}

#[test]
fn scaler_color_setup_preserves_rgb_pixels_and_alpha() {
    let mut decoded = Video::new(Pixel::RGBA, 8, 8);
    for pixel in decoded.data_mut(0).chunks_exact_mut(4) {
        pixel.copy_from_slice(&[23, 151, 202, 97]);
    }
    let expected = copy_rgba_frame(&decoded, 8, 8).unwrap();
    let (context, mut size) =
        StickyMonitor::make_scaler(Pixel::RGBA, 8, 8, 8, 8, ScalingQuality::Bicubic).unwrap();
    let converted = scale_monitor_frame(
        &mut Some(context),
        &mut Some((Pixel::RGBA, 8, 8)),
        &mut Some(ScalingQuality::Bicubic),
        &mut size,
        &decoded,
        false,
        (8, 8),
        ScalingQuality::Bicubic,
        &DecoderStageTimingAccumulators::default(),
    )
    .unwrap();
    assert_eq!(copy_rgba_frame(&converted, 8, 8).unwrap(), expected);
}
