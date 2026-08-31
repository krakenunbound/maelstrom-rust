//! Source-frame identity, not codec/color quality: preserve the production export graph while
//! substituting the redistributable MPEG-4 encoder, as in the existing five-color VFR test.
use super::*;
use crate::audio_boundary_tests::run_ffmpeg_bounded;
use nle_timeline::MediaId;
use nle_ui_core::{EditorState, Language, ProjectFrameRate, SourceFrameTimeIndex};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn decode_rgb(ffmpeg: &Path, source: &Path, raw: &Path, stderr: &Path) -> Vec<Vec<u8>> {
    run_ffmpeg_bounded(
        ffmpeg,
        &[
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
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
        ],
        stderr,
    );
    let bytes = fs::read(raw).expect("read independently decoded RGB frames");
    const FRAME_BYTES: usize = 320 * 180 * 3;
    assert!(!bytes.is_empty());
    assert_eq!(bytes.len() % FRAME_BYTES, 0);
    bytes
        .chunks_exact(FRAME_BYTES)
        .map(ToOwned::to_owned)
        .collect()
}

fn shifted_vfr_export_matches_preview(variable: &str, codec: &str) {
    let Some(source) = std::env::var_os(variable).map(PathBuf::from) else {
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
        std::env::temp_dir().join(format!("maelstrom-vfr-export-{codec}-{nonce}.{ext}"))
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
        format!("codec_name={codec}"),
        "width=320".into(),
        "height=180".into(),
        "pix_fmt=yuv422p10le".into(),
        "start_time=7.000000".into(),
    ] {
        assert!(
            metadata.lines().any(|line| line == required),
            "fixture contract: {metadata}"
        );
    }
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
    let pts: Vec<i64> = fs::read_to_string(timestamps)
        .unwrap()
        .lines()
        .map(|line| (line.parse::<f64>().unwrap() * 1_000_000.0).round() as i64 - 7_000_000)
        .collect();
    assert_eq!(
        pts,
        [
            0, 41_667, 125_000, 166_667, 250_000, 333_333, 458_333, 500_000
        ]
    );
    let references = decode_rgb(&ffmpeg, &source, raw, stderr);
    assert_eq!(references.len(), pts.len());

    for fps in [[30, 1], [30_000, 1_001]] {
        // Whole project-frame durations avoid inventing a partial-final-frame export policy.
        let frame_tick =
            |frame: u64| (frame * 1_000_000 * u64::from(fps[1])).div_ceil(u64::from(fps[0])) as i64;
        for (case, source_in, slip, frame_count) in [
            ("head", 0, 0, 6),
            ("trim", 100_000, 0, 6),
            ("slip", 100_000, 45_000, 6),
            ("exclusive-tail", 400_000, 0, 3),
            ("final-frame", 500_000, 0, 1),
        ] {
            let mut editor = EditorState::new_with_frame_rate(
                Language::English,
                "Shifted ten-bit VFR export",
                ProjectFrameRate::new(fps[0], fps[1]).unwrap(),
            );
            editor.add_media_paths([source.clone()]);
            editor.media[0].duration = Some(Tick(541_667));
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
                        "{codec}/{case}/{fps:?} preview frame {frame}"
                    );
                    index
                })
                .collect();
            let request = ExportRequest {
                snapshot: editor.snapshot(),
                settings: ProjectSettings {
                    fps,
                    size: [320, 180],
                },
                output: output.clone(),
                ffmpeg: ffmpeg.clone(),
                encoders: vec![H264Encoder::OpenH264],
            };
            let plan = ExportPlan::from_request(&request).unwrap();
            assert_eq!(plan.duration, Tick(frame_tick(frame_count)));
            let (mut args, graph) =
                build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
            let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
            args[encoder + 1] = "mpeg4".into();
            let script = args.iter().position(|arg| arg == "FILTER_SCRIPT").unwrap();
            args[script] = filter.to_string_lossy().into_owned();
            fs::write(filter, graph).unwrap();
            run_ffmpeg_bounded(&ffmpeg, &args, stderr);
            let frames = decode_rgb(&ffmpeg, output, raw, stderr);
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
                    "{codec}/{case}/{fps:?}: output frame {frame} has timestamp {pts}"
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
                "{codec}/{case}/{fps:?} exported source identities"
            );
            println!(
                "shifted VFR export: codec={codec}, case={case}, fps={fps:?}, identities={actual:?}"
            );
        }
    }
}

#[test]
fn supplied_prores_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview("MAELSTROM_PRORES_VFR_TEST_MEDIA", "prores");
}

#[test]
fn supplied_dnxhr_vfr_export_matches_preview_source_identity() {
    shifted_vfr_export_matches_preview("MAELSTROM_DNXHR_VFR_TEST_MEDIA", "dnxhd");
}
