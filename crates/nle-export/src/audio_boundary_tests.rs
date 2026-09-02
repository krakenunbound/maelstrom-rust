use super::*;
use std::{
    fs::{self, File},
    path::PathBuf,
    process::Stdio,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct TempFiles(Vec<PathBuf>);

struct BoundedChild(Option<Child>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn bundled_ffmpeg() -> Option<PathBuf> {
    let root = std::env::var_os("FFMPEG_DIR").map(PathBuf::from)?;
    let path = root.join("bin").join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    path.exists().then_some(path)
}

fn unique_temp_path(name: &str, extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "maelstrom-audio-boundary-{name}-{nonce}.{extension}"
    ))
}

pub(super) fn try_run_ffmpeg_bounded(
    ffmpeg: &Path,
    args: &[String],
    stderr: &Path,
) -> Result<(), String> {
    let stderr_file = File::create(stderr)
        .map_err(|error| format!("could not create FFmpeg stderr capture: {error}"))?;
    let mut child = BoundedChild(Some(
        Command::new(ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .map_err(|error| format!("could not start bundled FFmpeg: {error}"))?,
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child
            .0
            .as_mut()
            .expect("FFmpeg child must remain owned until it exits")
            .try_wait()
            .map_err(|error| format!("could not poll FFmpeg: {error}"))?
        {
            child.0.take();
            return status.success().then_some(()).ok_or_else(|| {
                format!(
                    "FFmpeg failed: {}",
                    fs::read_to_string(stderr).unwrap_or_default()
                )
            });
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "FFmpeg exceeded the 10-second audio-boundary watchdog: {}",
                fs::read_to_string(stderr).unwrap_or_default()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn run_ffmpeg_bounded(ffmpeg: &Path, args: &[String], stderr: &Path) {
    if let Err(error) = try_run_ffmpeg_bounded(ffmpeg, args, stderr) {
        panic!("{error}");
    }
}

fn decoded_samples(path: &Path) -> Vec<f32> {
    fs::read(path)
        .expect("read decoded PCM")
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
        .collect()
}

#[derive(Debug)]
struct AudioFrame {
    pts: i64,
    samples: usize,
}

fn ashowinfo_frames(stderr: &str, marker: &str) -> Vec<AudioFrame> {
    stderr
        .lines()
        .filter(|line| line.contains(marker))
        .map(|line| {
            let value_after = |field: &str| {
                line.split_once(field)
                    .and_then(|(_, value)| value.split_whitespace().next())
                    .unwrap_or_else(|| panic!("missing {field} in ashowinfo output: {line}"))
            };
            AudioFrame {
                pts: value_after("pts:")
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid output PTS in ashowinfo: {line}")),
                samples: value_after("nb_samples:")
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid nb_samples in ashowinfo: {line}")),
            }
        })
        .collect()
}

fn assert_sample_clock(frames: &[AudioFrame]) {
    assert!(!frames.is_empty(), "ashowinfo did not report output frames");
    assert_eq!(frames[0].pts, 0);
    assert!(frames.windows(2).all(|window| {
        window[1].pts == window[0].pts + i64::try_from(window[0].samples).unwrap()
    }));
    assert_eq!(
        frames.iter().map(|frame| frame.samples).sum::<usize>(),
        48_000
    );
}

#[test]
fn audio_output_boundary_rounds_up_and_never_emits_negative_sample_counts() {
    let cases = [
        (Tick(-1), 0_i128),
        (Tick(0), 0),
        (Tick(1), 1),
        (Tick(20), 1),
        (Tick(21), 2),
        (Tick(i64::MAX), 442_721_857_769_029_239),
    ];
    for (duration, samples) in cases {
        assert_eq!(
            audio_output_boundary(duration),
            format!("apad=whole_len={samples},atrim=end_sample={samples},asetpts=N/SR/TB")
        );
    }
}

#[test]
fn real_ffmpeg_audio_boundary_repairs_invalid_timestamps_and_preserves_timeline_gaps() {
    let _ffmpeg_guard = super::tests::real_ffmpeg_test_guard();
    let Some(ffmpeg) = bundled_ffmpeg() else {
        return;
    };
    for (name, bad_clock, upstream_invalid) in
        [("nan", "NAN", true), ("negative", "PTS-10/TB", false)]
    {
        let output = unique_temp_path(&format!("gap-tone-{name}"), "f32le");
        let stderr = unique_temp_path(&format!("gap-tone-{name}"), "stderr");
        let _cleanup = TempFiles(vec![output.clone(), stderr.clone()]);
        let boundary = audio_output_boundary(Tick(1_000_000));
        let graph = format!(
            "[0:a]adelay=9600S:all=1[left];[1:a]adelay=19200S:all=1[right];\
             [left][right]amix=inputs=2:normalize=0,asetpts={bad_clock},ashowinfo@upstream,\
             {boundary},ashowinfo@output[aout]"
        );
        let args = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "info".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=1000:sample_rate=48000:duration=0.1".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=500:sample_rate=48000:duration=0.1".into(),
            "-filter_complex".into(),
            graph,
            "-map".into(),
            "[aout]".into(),
            "-ac".into(),
            "1".into(),
            "-ar".into(),
            "48000".into(),
            "-f".into(),
            "f32le".into(),
            "-fs".into(),
            "1048576".into(),
            "-y".into(),
            output.display().to_string(),
        ];
        run_ffmpeg_bounded(&ffmpeg, &args, &stderr);

        let log = fs::read_to_string(&stderr).unwrap();
        let upstream = log
            .lines()
            .filter(|line| line.contains("[ashowinfo@upstream @"))
            .collect::<Vec<_>>();
        assert!(!upstream.is_empty(), "missing upstream ashowinfo: {log}");
        if upstream_invalid {
            assert!(upstream.iter().any(|line| line.contains("pts:NOPTS")));
        } else {
            assert!(upstream.iter().any(|line| {
                line.split_once("pts:")
                    .and_then(|(_, value)| value.split_whitespace().next())
                    .and_then(|value| value.parse::<i64>().ok())
                    .is_some_and(|pts| pts < 0)
            }));
        }
        let samples = decoded_samples(&output);
        assert_eq!(samples.len(), 48_000);
        assert!(samples[..9_600].iter().all(|sample| sample.abs() < 1e-6));
        assert!(
            samples[9_600..14_400]
                .iter()
                .any(|sample| sample.abs() > 0.01)
        );
        assert!(
            samples[14_400..19_200]
                .iter()
                .all(|sample| sample.abs() < 1e-6)
        );
        assert!(
            samples[19_200..24_000]
                .iter()
                .any(|sample| sample.abs() > 0.01)
        );
        assert!(samples[24_000..].iter().all(|sample| sample.abs() < 1e-6));
        assert_sample_clock(&ashowinfo_frames(&log, "[ashowinfo@output @"));
    }
}

#[test]
fn real_ffmpeg_audio_boundary_makes_an_empty_silence_stream_finite() {
    let _ffmpeg_guard = super::tests::real_ffmpeg_test_guard();
    let Some(ffmpeg) = bundled_ffmpeg() else {
        return;
    };
    let output = unique_temp_path("empty-silence", "f32le");
    let stderr = unique_temp_path("empty-silence", "stderr");
    let _cleanup = TempFiles(vec![output.clone(), stderr.clone()]);
    let boundary = audio_output_boundary(Tick(1_000_000));
    let graph = format!("[0:a]atrim=end_sample=0,asetpts=NAN,{boundary},ashowinfo@output[aout]");
    let args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "info".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "anullsrc=r=48000:cl=mono".into(),
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[aout]".into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "48000".into(),
        "-f".into(),
        "f32le".into(),
        "-fs".into(),
        "1048576".into(),
        "-y".into(),
        output.display().to_string(),
    ];
    run_ffmpeg_bounded(&ffmpeg, &args, &stderr);

    let samples = decoded_samples(&output);
    assert_eq!(samples.len(), 48_000);
    assert!(samples.iter().all(|sample| sample.abs() < 1e-6));
    assert_sample_clock(&ashowinfo_frames(
        &fs::read_to_string(&stderr).unwrap(),
        "[ashowinfo@output @",
    ));
}
