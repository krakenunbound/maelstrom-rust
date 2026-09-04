//! Source-frame identity, not codec/color quality: preserve the production export graph while
//! substituting the redistributable MPEG-4 encoder, as in the existing five-color VFR test.
use super::*;
use crate::audio_boundary_tests::{run_ffmpeg_bounded, try_run_ffmpeg_bounded};
use nle_timeline::MediaId;
use nle_ui_core::{EditorState, Language, ProjectFrameRate, SourceFrameTimeIndex};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempFiles(Vec<PathBuf>);

struct ShiftedVfrFixture {
    variable: &'static str,
    codec: &'static str,
    origin: i64,
    pix_fmt: &'static str,
    size: [u32; 2],
    local_pts: &'static [i64],
    source_duration: i64,
    exclusive_tail: (i64, u64),
    final_frame_source_in: i64,
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn decode_rgb(
    ffmpeg: &Path,
    source: &Path,
    raw: &Path,
    stderr: &Path,
    size: [u32; 2],
    decoder: Option<&str>,
) -> Vec<Vec<u8>> {
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    if let Some(decoder) = decoder {
        args.extend(["-c:v".into(), decoder.into()]);
    }
    args.extend([
        "-i".into(),
        source.to_string_lossy().into_owned(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-an".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        raw.to_string_lossy().into_owned(),
    ]);
    run_ffmpeg_bounded(ffmpeg, &args, stderr);
    let bytes = fs::read(raw).expect("read independently decoded RGB frames");
    let frame_bytes = size[0] as usize * size[1] as usize * 3;
    assert!(!bytes.is_empty());
    assert_eq!(bytes.len() % frame_bytes, 0);
    bytes
        .chunks_exact(frame_bytes)
        .map(ToOwned::to_owned)
        .collect()
}

fn av1_reference_decoder(
    ffprobe: &Path,
    source: &Path,
    timestamps: &Path,
    stderr: &Path,
) -> String {
    let mut failures = Vec::new();
    for decoder in ["av1_cuvid", "av1_qsv", "av1"] {
        let _ = fs::remove_file(timestamps);
        let args = [
            "-v".into(),
            "error".into(),
            "-c:v".into(),
            decoder.into(),
            "-select_streams".into(),
            "v:0".into(),
            "-show_entries".into(),
            "frame=best_effort_timestamp_time".into(),
            "-of".into(),
            "csv=p=0".into(),
            "-o".into(),
            timestamps.to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned(),
        ];
        match try_run_ffmpeg_bounded(ffprobe, &args, stderr) {
            Ok(()) if fs::metadata(timestamps).is_ok_and(|metadata| metadata.len() > 0) => {
                return decoder.to_owned();
            }
            Ok(()) => failures.push(format!("{decoder}: no decoded frame timestamps")),
            Err(error) => failures.push(format!("{decoder}: {error}")),
        }
    }
    panic!(
        "no independent AV1 decoder produced frame timestamps ({})",
        failures.join("; ")
    );
}

fn run_identity_export(
    ffmpeg: &Path,
    request: &ExportRequest,
    plan: &ExportPlan,
    filter: &Path,
    output: &Path,
    stderr: &Path,
    av1: bool,
) {
    let candidates = if av1 {
        av1_input_decoder_candidates(plan)
    } else {
        vec![Av1InputDecoder::Default]
    };
    let mut failures = Vec::new();
    for decoder in candidates {
        let (mut args, graph) = build_ffmpeg_job_with_title_assets_and_input_decoder(
            request,
            plan,
            H264Encoder::OpenH264,
            &[],
            decoder,
        )
        .unwrap();
        let encoder = args.iter().rposition(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".into();
        let script = args.iter().position(|arg| arg == "FILTER_SCRIPT").unwrap();
        args[script] = filter.to_string_lossy().into_owned();
        fs::write(filter, graph).unwrap();
        let _ = fs::remove_file(output);
        match try_run_ffmpeg_bounded(ffmpeg, &args, stderr) {
            Ok(()) => return,
            Err(error) => failures.push(format!("{}: {error}", decoder.name())),
        }
    }
    let label = if av1 {
        "all identity-export AV1 decoder candidates failed"
    } else {
        "identity export failed"
    };
    panic!("{label} ({})", failures.join("; "));
}

fn shifted_vfr_export_matches_preview(fixture: &ShiftedVfrFixture) {
    let Some(source) = std::env::var_os(fixture.variable).map(PathBuf::from) else {
        return;
    };
    let _guard = crate::tests::real_ffmpeg_test_guard();
    let root = PathBuf::from(
        std::env::var_os("FFMPEG_DIR").expect("supplied fixture requires FFMPEG_DIR"),
    );
    let ffmpeg = root.join("bin").join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let paths = ["properties", "pts", "rgb", "filter", "mp4", "stderr"].map(|ext| {
        std::env::temp_dir().join(format!(
            "maelstrom-vfr-export-{}-{nonce}.{ext}",
            fixture.codec
        ))
    });
    let _cleanup = TempFiles(paths.to_vec());
    let [properties, timestamps, raw, filter, output, stderr] = &paths;
    run_ffmpeg_bounded(
        &ffprobe,
        &[
            "-v".into(),
            "error".into(),
            "-select_streams".into(),
            "v:0".into(),
            "-show_entries".into(),
            "stream=codec_name,width,height,pix_fmt,start_time".into(),
            "-of".into(),
            "default=noprint_wrappers=1".into(),
            "-o".into(),
            properties.to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned(),
        ],
        stderr,
    );
    let metadata = fs::read_to_string(properties).unwrap();
    for required in [
        format!("codec_name={}", fixture.codec),
        format!("width={}", fixture.size[0]),
        format!("height={}", fixture.size[1]),
        format!("pix_fmt={}", fixture.pix_fmt),
        format!(
            "start_time={}.{:06}",
            fixture.origin / 1_000_000,
            fixture.origin % 1_000_000
        ),
    ] {
        assert!(
            metadata.lines().any(|line| line == required),
            "fixture contract: {metadata}"
        );
    }
    let reference_decoder = (fixture.codec == "av1")
        .then(|| av1_reference_decoder(&ffprobe, &source, timestamps, stderr));
    if reference_decoder.is_none() {
        run_ffmpeg_bounded(
            &ffprobe,
            &[
                "-v".into(),
                "error".into(),
                "-select_streams".into(),
                "v:0".into(),
                "-show_entries".into(),
                "frame=best_effort_timestamp_time".into(),
                "-of".into(),
                "csv=p=0".into(),
                "-o".into(),
                timestamps.to_string_lossy().into_owned(),
                source.to_string_lossy().into_owned(),
            ],
            stderr,
        );
    }
    let pts: Vec<i64> = fs::read_to_string(timestamps)
        .unwrap()
        .lines()
        .map(|line| (line.parse::<f64>().unwrap() * 1_000_000.0).round() as i64 - fixture.origin)
        .collect();
    assert_eq!(pts, fixture.local_pts);
    // AV1 references use FFmpeg's explicitly named native decoder, not packet order/timing.
    let references = decode_rgb(
        &ffmpeg,
        &source,
        raw,
        stderr,
        fixture.size,
        reference_decoder.as_deref(),
    );
    assert_eq!(references.len(), pts.len());

    for fps in [[30, 1], [30_000, 1_001]] {
        // Whole project-frame durations avoid inventing a partial-final-frame export policy.
        let frame_tick =
            |frame: u64| (frame * 1_000_000 * u64::from(fps[1])).div_ceil(u64::from(fps[0])) as i64;
        for (case, source_in, slip, frame_count) in [
            ("head", 0, 0, 6),
            ("trim", 100_000, 0, 6),
            ("slip", 100_000, 45_000, 6),
            (
                "exclusive-tail",
                fixture.exclusive_tail.0,
                0,
                fixture.exclusive_tail.1,
            ),
            ("final-frame", fixture.final_frame_source_in, 0, 1),
        ] {
            let mut editor = EditorState::new_with_frame_rate(
                Language::English,
                "Shifted VFR export",
                ProjectFrameRate::new(fps[0], fps[1]).unwrap(),
            );
            editor.add_media_paths([source.clone()]);
            editor.media[0].duration = Some(Tick(fixture.source_duration));
            editor.set_media_frame_time_index(
                1,
                Some(SourceFrameTimeIndex::new(pts.iter().copied().map(Tick).collect()).unwrap()),
            );
            let track = editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .unwrap()
                .id;
            let clip = editor
                .timeline
                .insert_clip(
                    track,
                    MediaId(1),
                    Tick(0),
                    Tick(frame_tick(frame_count)),
                    Tick(source_in),
                )
                .unwrap();
            if slip != 0 {
                editor.timeline.slip_clip(clip, Tick(slip), false).unwrap();
            }
            let expected: Vec<usize> = (0..frame_count)
                .map(|frame| {
                    let tick = frame_tick(frame);
                    let logical = source_in + slip + tick;
                    let index = pts.partition_point(|pts| *pts <= logical) - 1;
                    editor.set_playhead(Tick(tick));
                    let target = editor.playback_target().expect("active preview target");
                    assert_eq!(target.source_tick, Tick(logical));
                    assert_eq!(
                        target.decode_tick,
                        Tick(pts[index]),
                        "{}/{case}/{fps:?} preview frame {frame}",
                        fixture.codec
                    );
                    index
                })
                .collect();
            let request = ExportRequest {
                snapshot: editor.snapshot(),
                settings: ProjectSettings {
                    fps,
                    size: fixture.size,
                },
                output: output.clone(),
                ffmpeg: ffmpeg.clone(),
                encoders: vec![H264Encoder::OpenH264],
            };
            let plan = ExportPlan::from_request(&request).unwrap();
            assert_eq!(plan.duration, Tick(frame_tick(frame_count)));
            run_identity_export(
                &ffmpeg,
                &request,
                &plan,
                filter,
                output,
                stderr,
                fixture.codec == "av1",
            );
            let frames = decode_rgb(&ffmpeg, output, raw, stderr, fixture.size, None);
            run_ffmpeg_bounded(
                &ffprobe,
                &[
                    "-v".into(),
                    "error".into(),
                    "-select_streams".into(),
                    "v:0".into(),
                    "-show_entries".into(),
                    "frame=best_effort_timestamp_time".into(),
                    "-of".into(),
                    "csv=p=0".into(),
                    "-o".into(),
                    timestamps.to_string_lossy().into_owned(),
                    output.to_string_lossy().into_owned(),
                ],
                stderr,
            );
            let output_pts: Vec<i64> = fs::read_to_string(timestamps)
                .unwrap()
                .lines()
                .map(|line| (line.parse::<f64>().unwrap() * 1_000_000.0).round() as i64)
                .collect();
            assert_eq!(output_pts.len(), frame_count as usize);
            assert_eq!(output_pts[0], 0);
            for (frame, pts) in output_pts.iter().enumerate() {
                // FFprobe rounds its decimal output; the editor uses the first representable
                // microsecond. This is timestamp rounding tolerance, not a frame-identity tolerance.
                assert!(
                    (*pts - frame_tick(frame as u64)).abs() <= 1,
                    "{}/{case}/{fps:?}: output frame {frame} has timestamp {pts}",
                    fixture.codec
                );
            }
            let actual: Vec<usize> = frames
                .iter()
                .map(|frame| {
                    let mut candidates: Vec<(u64, usize)> = references
                        .iter()
                        .enumerate()
                        .map(|(index, reference)| {
                            let error = frame
                                .iter()
                                .zip(reference)
                                .map(|(a, b)| {
                                    let delta = i64::from(*a) - i64::from(*b);
                                    (delta * delta) as u64
                                })
                                .sum();
                            (error, index)
                        })
                        .collect();
                    candidates.sort_unstable();
                    assert!(
                        candidates[0].0 < candidates[1].0,
                        "ambiguous source identity"
                    );
                    candidates[0].1
                })
                .collect();
            assert_eq!(
                actual, expected,
                "{}/{case}/{fps:?} exported source identities",
                fixture.codec
            );
            println!(
                "shifted VFR export: codec={}, case={case}, fps={fps:?}, identities={actual:?}",
                fixture.codec
            );
        }
    }
}

#[test]
fn supplied_prores_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview(&ShiftedVfrFixture {
        variable: "MAELSTROM_PRORES_VFR_TEST_MEDIA",
        codec: "prores",
        origin: 7_000_000,
        pix_fmt: "yuv422p10le",
        size: [320, 180],
        local_pts: &[
            0, 41_667, 125_000, 166_667, 250_000, 333_333, 458_333, 500_000,
        ],
        source_duration: 541_667,
        exclusive_tail: (400_000, 3),
        final_frame_source_in: 500_000,
    });
}

#[test]
fn supplied_dnxhr_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview(&ShiftedVfrFixture {
        variable: "MAELSTROM_DNXHR_VFR_TEST_MEDIA",
        codec: "dnxhd",
        origin: 7_000_000,
        pix_fmt: "yuv422p10le",
        size: [320, 180],
        local_pts: &[
            0, 41_667, 125_000, 166_667, 250_000, 333_333, 458_333, 500_000,
        ],
        source_duration: 541_667,
        exclusive_tail: (400_000, 3),
        final_frame_source_in: 500_000,
    });
}

#[test]
fn supplied_shifted_reordered_mpeg4_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview(&ShiftedVfrFixture {
        variable: "MAELSTROM_SHIFTED_REORDERED_VFR_TEST_MEDIA",
        codec: "mpeg4",
        origin: 3_000_000,
        pix_fmt: "yuv420p",
        size: [320, 180],
        local_pts: &[
            0, 33_333, 100_000, 133_333, 200_000, 266_667, 366_667, 400_000,
        ],
        source_duration: 433_333,
        exclusive_tail: (266_667, 3),
        final_frame_source_in: 400_000,
    });
}

#[test]
fn generated_shifted_ffv1_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview(&ShiftedVfrFixture {
        variable: "MAELSTROM_FFV1_VFR_TEST_MEDIA",
        codec: "ffv1",
        origin: 9_000_000,
        pix_fmt: "yuv420p",
        size: [320, 180],
        local_pts: &[
            0, 42_000, 125_000, 167_000, 250_000, 333_000, 458_000, 500_000,
        ],
        source_duration: 542_000,
        exclusive_tail: (333_000, 3),
        final_frame_source_in: 500_000,
    });
}

#[test]
fn generated_shifted_ffvhuff_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview(&ShiftedVfrFixture {
        variable: "MAELSTROM_FFVHUFF_VFR_TEST_MEDIA",
        codec: "ffvhuff",
        origin: 5_000_000,
        pix_fmt: "bgra",
        size: [160, 90],
        local_pts: &[
            0, 42_000, 125_000, 167_000, 250_000, 333_000, 458_000, 500_000,
        ],
        source_duration: 542_000,
        exclusive_tail: (333_000, 3),
        final_frame_source_in: 500_000,
    });
}

#[test]
fn supplied_av1_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview(&ShiftedVfrFixture {
        variable: "MAELSTROM_AV1_VFR_TEST_MEDIA",
        codec: "av1",
        origin: 5_000_000,
        pix_fmt: "yuv420p",
        size: [352, 288],
        local_pts: &[
            0, 33_000, 100_000, 133_000, 200_000, 267_000, 367_000, 400_000,
        ],
        source_duration: 433_000,
        // At 30 fps this selects whole project frames through the 367 ms source frame.
        exclusive_tail: (267_000, 4),
        final_frame_source_in: 400_000,
    });
}
