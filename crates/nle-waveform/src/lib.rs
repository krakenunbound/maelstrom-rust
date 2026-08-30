//! Bounded FFmpeg waveform analysis for background media workers.
//!
//! [`analyze_path`] probes with `ffprobe` and streams decoded mono `f32le`
//! samples from `ffmpeg`. It blocks on process I/O and CPU work, so it **must
//! never run on the UI thread**. This process seam will be replaced by sticky
//! linked FFmpeg contexts when monitor playback is introduced.

use std::{
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use serde::Deserialize;

/// The largest accepted number of output bins. This prevents caller-controlled
/// allocation from turning a malformed project into an unbounded allocation.
pub const MAX_TARGET_BINS: usize = 65_536;

/// A grid atlas keeps dense scrub samples within the fixed memory budget
/// instead of creating an impractically wide image.
pub const MAX_VIDEO_STRIP_FRAMES: usize = 1024;
/// Preview tiles taller than this do not add useful detail at timeline scale.
pub const MAX_VIDEO_STRIP_HEIGHT: u32 = 512;
const MAX_VIDEO_STRIP_BYTES: usize = 64 * 1024 * 1024;

const FFMPEG: &str = "ffmpeg";
const FFPROBE: &str = "ffprobe";
const DECODE_BUFFER_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// The timing index stores signed microsecond source ticks, so one million
/// points consumes roughly 8 MiB while still covering several hours of common
/// frame rates. Decoded timestamp output is streamed; this is only the retained index.
pub const MAX_FRAME_TIMING_POINTS: usize = 1_000_000;
const MAX_FRAME_TIMING_LINE_BYTES: usize = 4 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(8);
const FRAME_TIMING_PROBE_ARGS: [&str; 11] = [
    "-v",
    "error",
    "-threads",
    "1",
    "-select_streams",
    "v:0",
    "-show_frames",
    "-show_entries",
    "frame=best_effort_timestamp_time",
    "-of",
    "compact=p=0:nk=0",
];

fn media_tool(name: &'static str) -> PathBuf {
    media_tool_from_executable(name, std::env::current_exe().ok().as_deref())
}

fn media_tool_from_executable(name: &'static str, current_executable: Option<&Path>) -> PathBuf {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    current_executable
        .and_then(|path| path.parent().map(|parent| parent.join(&executable)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(executable))
}

/// The normalized extrema for one equally sized interval of decoded audio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Peak {
    /// Lowest downmixed sample in `[-1.0, 1.0]`.
    pub min: f32,
    /// Highest downmixed sample in `[-1.0, 1.0]`.
    pub max: f32,
}

/// A fixed-memory summary of an audio track suitable for drawing a timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Waveform {
    /// One normalized min/max pair per requested visual bin.
    pub peaks: Vec<Peak>,
    /// Audio sample rate reported by ffprobe, if the stream provided one.
    pub sample_rate: Option<u32>,
    /// Source channel count reported by ffprobe, if known.
    pub channels: Option<usize>,
    /// Decoded mono frame count before binning.
    pub total_frames: u64,
    /// `total_frames / sample_rate`, when a sample rate is known and non-zero.
    pub duration_seconds: Option<f64>,
}

/// Selected container, video, and audio facts reported by FFprobe.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaMetadata {
    pub duration_seconds: Option<f64>,
    pub file_size: Option<u64>,
    pub container: Option<String>,
    pub overall_bit_rate: Option<u64>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    /// The exact positive `avg_frame_rate` reported by FFprobe, reduced to lowest terms.
    pub frame_rate_ratio: Option<FrameRate>,
    pub video_bit_rate: Option<u64>,
    pub audio_codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<usize>,
    pub audio_bit_rate: Option<u64>,
    pub streams: Vec<MediaStreamMetadata>,
}

/// An exact positive frame rate reported by FFprobe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRate {
    numerator: u64,
    denominator: u64,
}

/// Complete source-frame presentation timestamps in the decoder's microsecond
/// time base. This is runtime data and intentionally has no serialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameTimeIndex {
    pts_microseconds: Vec<i64>,
}

impl FrameTimeIndex {
    /// Number of indexed presentation timestamps.
    pub fn len(&self) -> usize {
        self.pts_microseconds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts_microseconds.is_empty()
    }

    /// Sorted presentation timestamps in microseconds.
    pub fn pts(&self) -> &[i64] {
        &self.pts_microseconds
    }

    /// Consumes the index and returns its sorted microsecond timestamps.
    pub fn into_pts(self) -> Vec<i64> {
        self.pts_microseconds
    }
}

/// Frame-timing classification derived from a complete decoded-frame timestamp scan.
///
/// `Variable` is the only state that publishes a frame index. `Unknown` means
/// the scan produced too little or unusable timing data and must not be used
/// for CFR snapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameTiming {
    Constant,
    Variable(FrameTimeIndex),
    Unknown,
}

impl FrameRate {
    pub fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if numerator == 0 || denominator == 0 {
            return None;
        }
        let divisor = gcd(numerator, denominator);
        Some(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub const fn numerator(self) -> u64 {
        self.numerator
    }

    pub const fn denominator(self) -> u64 {
        self.denominator
    }
}

/// One FFmpeg stream as shown by the inspector. Keeping every stream avoids hiding alternate
/// audio, subtitle, attachment, or data tracks behind a first-stream summary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaStreamMetadata {
    pub index: usize,
    pub kind: Option<String>,
    pub codec: Option<String>,
    pub start_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub time_base: Option<String>,
    pub bit_rate: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub frame_rate_ratio: Option<FrameRate>,
    pub sample_rate: Option<u32>,
    pub channels: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeDocument {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: FfprobeFormat,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeStream {
    index: Option<usize>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<usize>,
    bit_rate: Option<String>,
    start_time: Option<String>,
    duration: Option<String>,
    time_base: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

/// An RGBA grid atlas composed of evenly sampled video frames.
///
/// It is deliberately an image rather than a collection of decoded frames so
/// the media catalog can cache it without retaining a decoder or GPU resource.
/// Tiles are row-major. Any cells after `frame_count` in the final row are
/// deterministic opaque-black padding emitted by FFmpeg's `tile` filter.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoStrip {
    /// Pixel dimensions of the complete atlas.
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Source duration used to distribute the sampled preview frames.
    pub duration_seconds: f64,
    /// Number of source preview frames requested; excludes final-row padding.
    pub frame_count: usize,
    /// Pixel dimensions of every atlas cell.
    pub frame_width: u32,
    pub frame_height: u32,
    /// Row-major atlas dimensions in cells.
    pub columns: usize,
    pub rows: usize,
}

/// An honest failure from waveform analysis.
#[derive(Debug)]
pub enum WaveformError {
    /// The requested bin count was zero or would allocate too much memory.
    InvalidTargetBins { requested: usize, maximum: usize },
    /// The caller requested a timeline preview that exceeds its fixed budget.
    InvalidVideoStrip {
        frame_count: usize,
        frame_height: u32,
        maximum_frames: usize,
        maximum_height: u32,
    },
    /// The resulting RGBA preview atlas would exceed its allocation budget.
    VideoStripTooLarge {
        width: u32,
        height: u32,
        maximum_bytes: usize,
    },
    /// The file could not be opened or read before invoking FFmpeg.
    Io { path: PathBuf, source: io::Error },
    /// `ffprobe` or `ffmpeg` was not available on `PATH`.
    ExecutableUnavailable {
        executable: &'static str,
        source: io::Error,
    },
    /// ffprobe could not inspect the container or stream metadata.
    Probe { path: PathBuf, message: String },
    /// No audio stream, or no decoded audio frames, was found.
    NoAudio { path: PathBuf },
    /// No decodable video stream was found.
    NoVideo { path: PathBuf },
    /// FFmpeg could not decode the selected audio stream.
    Decode { path: PathBuf, message: String },
    /// FFmpeg could not decode or assemble the requested video preview.
    VideoDecode { path: PathBuf, message: String },
    /// FFprobe could not produce a complete bounded decoded video frame timing scan.
    FrameTiming { path: PathBuf, message: String },
    /// The owning media-analysis job was superseded and its subprocesses were reaped.
    Cancelled { path: PathBuf },
}

impl fmt::Display for WaveformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTargetBins { requested, maximum } => {
                write!(
                    f,
                    "requested {requested} waveform bins; maximum is {maximum}"
                )
            }
            Self::InvalidVideoStrip {
                frame_count,
                frame_height,
                maximum_frames,
                maximum_height,
            } => write!(
                f,
                "requested video strip ({frame_count} frames at {frame_height}px); maximum is {maximum_frames} frames at {maximum_height}px"
            ),
            Self::VideoStripTooLarge {
                width,
                height,
                maximum_bytes,
            } => write!(
                f,
                "video preview atlas {width}x{height} exceeds the {maximum_bytes}-byte budget"
            ),
            Self::Io { path, source } => write!(f, "could not read {}: {source}", path.display()),
            Self::ExecutableUnavailable { executable, source } => {
                write!(f, "{executable} is unavailable: {source}")
            }
            Self::Probe { path, message } => {
                write!(f, "could not probe audio in {}: {message}", path.display())
            }
            Self::NoAudio { path } => write!(f, "no decodable audio track in {}", path.display()),
            Self::NoVideo { path } => write!(f, "no decodable video track in {}", path.display()),
            Self::Decode { path, message } => {
                write!(f, "could not decode audio in {}: {message}", path.display())
            }
            Self::VideoDecode { path, message } => {
                write!(f, "could not decode video in {}: {message}", path.display())
            }
            Self::FrameTiming { path, message } => {
                write!(
                    f,
                    "could not scan frame timing in {}: {message}",
                    path.display()
                )
            }
            Self::Cancelled { path } => {
                write!(f, "media analysis cancelled for {}", path.display())
            }
        }
    }
}

/// Extract a bounded, cached visual summary of a video's first stream.
///
/// FFmpeg samples the clip evenly at `frame_count / duration_seconds`, fits
/// each sample into a 16:9 timeline tile, then assembles a near-square,
/// row-major raw RGBA atlas. The final row is padded with opaque black tiles
/// when it is not full. This blocks on process I/O and must run on a media
/// worker, never on the UI thread.
pub fn extract_video_strip(
    path: impl AsRef<Path>,
    duration_seconds: f64,
    frame_count: usize,
    frame_height: u32,
) -> Result<VideoStrip, WaveformError> {
    extract_video_strip_cancellable(
        path,
        duration_seconds,
        frame_count,
        frame_height,
        Arc::new(AtomicBool::new(false)),
    )
}

/// Cancellable variant of [`extract_video_strip`].  Set `cancelled` when this
/// import has been superseded; the FFmpeg child is killed, waited, and all I/O
/// reader threads are joined before this returns.
pub fn extract_video_strip_cancellable(
    path: impl AsRef<Path>,
    duration_seconds: f64,
    frame_count: usize,
    frame_height: u32,
    cancelled: Arc<AtomicBool>,
) -> Result<VideoStrip, WaveformError> {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return Err(WaveformError::InvalidVideoStrip {
            frame_count,
            frame_height,
            maximum_frames: MAX_VIDEO_STRIP_FRAMES,
            maximum_height: MAX_VIDEO_STRIP_HEIGHT,
        });
    }
    if frame_count == 0
        || frame_count > MAX_VIDEO_STRIP_FRAMES
        || frame_height == 0
        || frame_height > MAX_VIDEO_STRIP_HEIGHT
    {
        return Err(WaveformError::InvalidVideoStrip {
            frame_count,
            frame_height,
            maximum_frames: MAX_VIDEO_STRIP_FRAMES,
            maximum_height: MAX_VIDEO_STRIP_HEIGHT,
        });
    }

    let path = path.as_ref();
    fs::metadata(path).map_err(|source| WaveformError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }

    let frame_width = frame_height.saturating_mul(16).div_ceil(9);
    let (columns, rows) = video_atlas_dimensions(frame_count);
    let width = u32::try_from(columns)
        .ok()
        .and_then(|columns| frame_width.checked_mul(columns))
        .ok_or(WaveformError::VideoStripTooLarge {
            width: u32::MAX,
            height: u32::MAX,
            maximum_bytes: MAX_VIDEO_STRIP_BYTES,
        })?;
    let height = u32::try_from(rows)
        .ok()
        .and_then(|rows| frame_height.checked_mul(rows))
        .ok_or(WaveformError::VideoStripTooLarge {
            width,
            height: u32::MAX,
            maximum_bytes: MAX_VIDEO_STRIP_BYTES,
        })?;
    let expected_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(height as usize))
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_VIDEO_STRIP_BYTES)
        .ok_or(WaveformError::VideoStripTooLarge {
            width,
            height,
            maximum_bytes: MAX_VIDEO_STRIP_BYTES,
        })?;

    let frames_per_second = frame_count as f64 / duration_seconds;
    let filter = format!(
        "fps={frames_per_second:.9}:round=up,scale={frame_width}:{frame_height}:force_original_aspect_ratio=decrease,pad={frame_width}:{frame_height}:(ow-iw)/2:(oh-ih)/2:color=black,tile={columns}x{rows}:padding=0:margin=0"
    );
    let mut command = Command::new(media_tool(FFMPEG));
    hide_console_window(&mut command);
    let mut child = command
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            &filter,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| executable_error(FFMPEG, source))?;
    let stdout = child.stdout.take().expect("piped stdout must be available");
    let stderr_worker = drain_stderr(child.stderr.take().expect("piped stderr must be available"));
    let stdout_worker = read_to_end(stdout);
    let status = match wait_for_child(&mut child, &cancelled, path) {
        Ok(status) => status,
        Err(error) => {
            let _ = join_stdout(stdout_worker);
            let _ = join_stderr(stderr_worker);
            return Err(error);
        }
    };
    let rgba = match join_stdout(stdout_worker) {
        Ok(rgba) => rgba,
        Err(source) => {
            let stderr = join_stderr(stderr_worker);
            return Err(WaveformError::VideoDecode {
                path: path.to_path_buf(),
                message: if stderr.is_empty() {
                    source.to_string()
                } else {
                    stderr_message(&stderr)
                },
            });
        }
    };
    let stderr = join_stderr(stderr_worker);
    if !status.success() {
        let message = stderr_message(&stderr);
        return Err(if message.contains("matches no streams") {
            WaveformError::NoVideo {
                path: path.to_path_buf(),
            }
        } else {
            WaveformError::VideoDecode {
                path: path.to_path_buf(),
                message,
            }
        });
    }
    if rgba.len() != expected_bytes {
        return Err(WaveformError::VideoDecode {
            path: path.to_path_buf(),
            message: format!(
                "FFmpeg emitted {} bytes for a {expected_bytes}-byte bounded RGBA strip",
                rgba.len()
            ),
        });
    }

    Ok(VideoStrip {
        width,
        height,
        rgba,
        duration_seconds,
        frame_count,
        frame_width,
        frame_height,
        columns,
        rows,
    })
}

/// Returns a compact grid which holds `frame_count` row-major tiles.
///
/// The smallest square-ish column count is used so the atlas stays practical
/// for both CPU copies and GPU texture uploads.
fn video_atlas_dimensions(frame_count: usize) -> (usize, usize) {
    debug_assert!(frame_count > 0);
    let columns = (1..=frame_count)
        .find(|columns| columns.saturating_mul(*columns) >= frame_count)
        .expect("positive frame count has a square root");
    (columns, frame_count.div_ceil(columns))
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn wait_for_child(
    child: &mut Child,
    cancelled: &AtomicBool,
    path: &Path,
) -> Result<ExitStatus, WaveformError> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            terminate_child(child);
            return Err(WaveformError::Cancelled {
                path: path.to_path_buf(),
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
            Err(source) => {
                terminate_child(child);
                return Err(WaveformError::Decode {
                    path: path.to_path_buf(),
                    message: source.to_string(),
                });
            }
        }
    }
}

fn read_to_end(mut stdout: ChildStdout) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_stdout(worker: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    worker
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("stdout reader panicked")))
}

fn join_stream_stdout(worker: thread::JoinHandle<io::Result<()>>) -> io::Result<()> {
    worker
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("stdout reader panicked")))
}

fn stream_stdout(
    mut stdout: ChildStdout,
) -> (
    Receiver<io::Result<Vec<u8>>>,
    thread::JoinHandle<io::Result<()>>,
) {
    let (sender, receiver) = mpsc::sync_channel(2);
    let worker = thread::spawn(move || {
        let mut buffer = vec![0_u8; DECODE_BUFFER_BYTES];
        loop {
            let read = stdout.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            if sender.send(Ok(buffer[..read].to_vec())).is_err() {
                return Ok(());
            }
        }
    });
    (receiver, worker)
}

fn receive_chunk(
    chunks: &Receiver<io::Result<Vec<u8>>>,
    child: &mut Child,
    cancelled: &AtomicBool,
    path: &Path,
) -> Result<Option<Vec<u8>>, WaveformError> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            terminate_child(child);
            return Err(WaveformError::Cancelled {
                path: path.to_path_buf(),
            });
        }
        match child.try_wait() {
            Ok(_) => {}
            Err(source) => {
                terminate_child(child);
                return Err(WaveformError::Decode {
                    path: path.to_path_buf(),
                    message: source.to_string(),
                });
            }
        }
        match chunks.recv_timeout(PROCESS_POLL_INTERVAL) {
            Ok(Ok(chunk)) => return Ok(Some(chunk)),
            Ok(Err(source)) => {
                return Err(WaveformError::Decode {
                    path: path.to_path_buf(),
                    message: source.to_string(),
                });
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

fn drain_stderr(mut stderr: ChildStderr) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut retained = Vec::with_capacity(MAX_STDERR_BYTES);
        let mut buffer = [0_u8; 4 * 1024];
        while let Ok(read) = stderr.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let remaining = MAX_STDERR_BYTES.saturating_sub(retained.len());
            retained.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        retained
    })
}

fn join_stderr(worker: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    worker.join().unwrap_or_default()
}

impl std::error::Error for WaveformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::ExecutableUnavailable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Decode `path` into a fixed number of normalized peak bins.
///
/// `ffprobe` discovers the first audio stream before two streaming FFmpeg
/// passes. The first measures mono frames; the second maps frames to bins. No
/// decoded media is retained, and this function must run on a background worker.
pub fn analyze_path(path: impl AsRef<Path>, target_bins: usize) -> Result<Waveform, WaveformError> {
    analyze_path_cancellable(path, target_bins, Arc::new(AtomicBool::new(false)))
}

/// Cancellable variant of [`analyze_path`]. The supplied token is observed
/// while probing and while both decode passes stream samples.
pub fn analyze_path_cancellable(
    path: impl AsRef<Path>,
    target_bins: usize,
    cancelled: Arc<AtomicBool>,
) -> Result<Waveform, WaveformError> {
    if target_bins == 0 || target_bins > MAX_TARGET_BINS {
        return Err(WaveformError::InvalidTargetBins {
            requested: target_bins,
            maximum: MAX_TARGET_BINS,
        });
    }

    let path = path.as_ref();
    fs::metadata(path).map_err(|source| WaveformError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }
    let probe = probe_audio(path, &cancelled)?;
    let total_frames = stream_decode(path, &cancelled, |_| {})?;
    if total_frames == 0 {
        return Err(WaveformError::NoAudio {
            path: path.to_path_buf(),
        });
    }

    let mut peaks = vec![
        Peak {
            min: 1.0,
            max: -1.0
        };
        target_bins
    ];
    let mut frame_index = 0_u64;
    let decoded_frames = stream_decode(path, &cancelled, |sample| {
        let bin = (((frame_index as u128 * target_bins as u128) / total_frames as u128) as usize)
            .min(target_bins - 1);
        let peak = &mut peaks[bin];
        peak.min = peak.min.min(sample);
        peak.max = peak.max.max(sample);
        frame_index = frame_index.saturating_add(1);
    })?;
    if decoded_frames == 0 {
        return Err(WaveformError::NoAudio {
            path: path.to_path_buf(),
        });
    }

    for peak in &mut peaks {
        if peak.min > peak.max {
            *peak = Peak { min: 0.0, max: 0.0 };
        }
    }

    Ok(Waveform {
        peaks,
        sample_rate: probe.sample_rate,
        channels: probe.channels,
        total_frames,
        duration_seconds: probe
            .sample_rate
            .filter(|rate| *rate != 0)
            .map(|rate| total_frames as f64 / f64::from(rate))
            .or(probe.duration_seconds),
    })
}

/// Probe the container duration with FFmpeg without decoding media.
/// This still performs process I/O and belongs on a media worker.
pub fn probe_duration(path: impl AsRef<Path>) -> Result<f64, WaveformError> {
    probe_duration_cancellable(path, Arc::new(AtomicBool::new(false)))
}

/// Cancellable variant of [`probe_duration`]. The FFprobe child is always
/// reaped before cancellation is returned.
pub fn probe_duration_cancellable(
    path: impl AsRef<Path>,
    cancelled: Arc<AtomicBool>,
) -> Result<f64, WaveformError> {
    let path = path.as_ref();
    fs::metadata(path).map_err(|source| WaveformError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }
    let mut command = Command::new(media_tool(FFPROBE));
    hide_console_window(&mut command);
    let mut child = command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| executable_error(FFPROBE, source))?;
    let stdout_worker = read_to_end(child.stdout.take().expect("piped stdout must be available"));
    let stderr_worker = drain_stderr(child.stderr.take().expect("piped stderr must be available"));
    let status = match wait_for_child(&mut child, &cancelled, path) {
        Ok(status) => status,
        Err(error) => {
            let _ = join_stdout(stdout_worker);
            let _ = join_stderr(stderr_worker);
            return Err(error);
        }
    };
    let stdout = join_stdout(stdout_worker).map_err(|source| WaveformError::Probe {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    let stderr = join_stderr(stderr_worker);
    if !status.success() {
        return Err(WaveformError::Probe {
            path: path.to_path_buf(),
            message: stderr_message(&stderr),
        });
    }
    String::from_utf8_lossy(&stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .ok_or_else(|| WaveformError::Probe {
            path: path.to_path_buf(),
            message: "FFprobe did not report a positive container duration".to_owned(),
        })
}

/// Scan the best video stream's decoded-frame presentation timestamps. This
/// blocks on process I/O and belongs on a media worker.
pub fn analyze_frame_timing(path: impl AsRef<Path>) -> Result<FrameTiming, WaveformError> {
    analyze_frame_timing_cancellable(path, Arc::new(AtomicBool::new(false)))
}

/// Cancellable variant of [`analyze_frame_timing`]. The FFprobe child and its
/// bounded stdout/stderr reader threads are always reaped before this returns.
pub fn analyze_frame_timing_cancellable(
    path: impl AsRef<Path>,
    cancelled: Arc<AtomicBool>,
) -> Result<FrameTiming, WaveformError> {
    let path = path.as_ref();
    fs::metadata(path).map_err(|source| WaveformError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }

    let mut command = Command::new(media_tool(FFPROBE));
    hide_console_window(&mut command);
    let mut child = command
        .args(FRAME_TIMING_PROBE_ARGS)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| executable_error(FFPROBE, source))?;
    let (chunks, stdout_worker) = stream_stdout(child.stdout.take().expect("piped stdout"));
    let stderr_worker = drain_stderr(child.stderr.take().expect("piped stderr"));
    let mut pts = Vec::new();
    let mut pending = Vec::new();

    let scan = 'scan: loop {
        let chunk = match receive_chunk(&chunks, &mut child, &cancelled, path) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break Ok(()),
            Err(WaveformError::Cancelled { .. }) => {
                break Err(WaveformError::Cancelled {
                    path: path.to_path_buf(),
                });
            }
            Err(error) => break Err(frame_timing_error(path, error.to_string())),
        };
        pending.extend_from_slice(&chunk);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=newline).collect::<Vec<_>>();
            let pts_time = match parse_frame_timestamp_line(&line) {
                Ok(Some(pts_time)) => pts_time,
                Ok(None) => continue,
                Err(message) => break 'scan Err(frame_timing_error(path, message)),
            };
            if let Err(message) = retain_frame_timing_point(&mut pts, pts_time) {
                break 'scan Err(frame_timing_error(path, message));
            }
        }
        if pending.len() > MAX_FRAME_TIMING_LINE_BYTES {
            break Err(frame_timing_error(
                path,
                "FFprobe emitted an oversized decoded frame timing line",
            ));
        }
    };

    if scan.is_err() {
        terminate_child(&mut child);
    }
    drop(chunks);
    let stdout_result = join_stream_stdout(stdout_worker);
    let stderr = join_stderr(stderr_worker);
    scan?;
    if let Err(source) = stdout_result {
        return Err(frame_timing_error(path, source.to_string()));
    }
    let status = wait_for_child(&mut child, &cancelled, path).map_err(|error| match error {
        WaveformError::Cancelled { .. } => WaveformError::Cancelled {
            path: path.to_path_buf(),
        },
        error => frame_timing_error(path, error.to_string()),
    })?;
    if !status.success() {
        return Err(frame_timing_error(path, stderr_message(&stderr)));
    }
    if !pending.is_empty() {
        let pts_time = parse_frame_timestamp_line(&pending)
            .map_err(|message| frame_timing_error(path, message))?;
        if let Some(pts_time) = pts_time {
            retain_frame_timing_point(&mut pts, pts_time)
                .map_err(|message| frame_timing_error(path, message))?;
        }
    }
    Ok(classify_frame_timing(pts))
}

fn retain_frame_timing_point(pts: &mut Vec<i64>, pts_time: i64) -> Result<(), String> {
    if pts.len() == MAX_FRAME_TIMING_POINTS {
        return Err(format!(
            "decoded video frame timing exceeds the {MAX_FRAME_TIMING_POINTS}-point safety cap"
        ));
    }
    pts.push(pts_time);
    Ok(())
}

fn frame_timing_error(path: &Path, message: impl Into<String>) -> WaveformError {
    WaveformError::FrameTiming {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn parse_frame_timestamp_line(line: &[u8]) -> Result<Option<i64>, String> {
    let line = std::str::from_utf8(line)
        .map_err(|_| "FFprobe emitted non-UTF-8 decoded frame timing output".to_owned())?
        .trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mut pts_time = None;
    for field in line.split('|') {
        let Some((key, value)) = field.split_once('=') else {
            return Err("FFprobe emitted malformed compact decoded frame timing output".to_owned());
        };
        if key == "best_effort_timestamp_time" && pts_time.replace(value).is_some() {
            return Err("FFprobe emitted duplicate best_effort_timestamp_time fields".to_owned());
        }
    }
    parse_pts_microseconds(pts_time.ok_or_else(|| {
        "FFprobe decoded frame output omitted best_effort_timestamp_time".to_owned()
    })?)
    .map(Some)
}

/// Converts FFprobe's decimal seconds to the decoder's integer microsecond
/// tick using nearest-integer rational rounding.
fn parse_pts_microseconds(value: &str) -> Result<i64, String> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, value),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid decoded frame timestamp {value:?}"));
    }
    let whole = whole
        .parse::<u128>()
        .map_err(|_| format!("decoded frame timestamp is out of range: {value:?}"))?;
    let denominator = 10_u128
        .checked_pow(fraction.len() as u32)
        .ok_or_else(|| format!("decoded frame timestamp has excessive precision: {value:?}"))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u128>()
            .map_err(|_| format!("decoded frame timestamp is out of range: {value:?}"))?
    };
    let numerator = whole
        .checked_mul(denominator)
        .and_then(|value| value.checked_add(fraction))
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or_else(|| format!("decoded frame timestamp is out of range: {value:?}"))?;
    let rounded = numerator
        .checked_add(denominator / 2)
        .ok_or_else(|| format!("decoded frame timestamp is out of range: {value:?}"))?
        / denominator;
    let magnitude = i64::try_from(rounded)
        .map_err(|_| format!("decoded frame timestamp is out of range: {value:?}"))?;
    Ok(if negative { -magnitude } else { magnitude })
}

fn classify_frame_timing(mut pts_microseconds: Vec<i64>) -> FrameTiming {
    if pts_microseconds.len() < 2 {
        return FrameTiming::Unknown;
    }
    pts_microseconds.sort_unstable();
    if pts_microseconds.windows(2).any(|pair| pair[0] >= pair[1]) {
        return FrameTiming::Unknown;
    }
    let origin = pts_microseconds[0];
    for pts in &mut pts_microseconds {
        let Some(local_pts) = pts.checked_sub(origin) else {
            return FrameTiming::Unknown;
        };
        *pts = local_pts;
    }
    let mut gaps = pts_microseconds.windows(2).map(|pair| pair[1] - pair[0]);
    let Some(first_gap) = gaps.next() else {
        return FrameTiming::Unknown;
    };
    let (min_gap, max_gap) = gaps.fold((first_gap, first_gap), |(min_gap, max_gap), gap| {
        (min_gap.min(gap), max_gap.max(gap))
    });
    if max_gap - min_gap <= 1 {
        FrameTiming::Constant
    } else {
        FrameTiming::Variable(FrameTimeIndex { pts_microseconds })
    }
}

/// Probe displayable container and all stream metadata without decoding media.
pub fn probe_media_metadata(path: impl AsRef<Path>) -> Result<MediaMetadata, WaveformError> {
    probe_media_metadata_cancellable(path, Arc::new(AtomicBool::new(false)))
}

/// Cancellable variant of [`probe_media_metadata`].
pub fn probe_media_metadata_cancellable(
    path: impl AsRef<Path>,
    cancelled: Arc<AtomicBool>,
) -> Result<MediaMetadata, WaveformError> {
    let path = path.as_ref();
    fs::metadata(path).map_err(|source| WaveformError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }
    let mut command = Command::new(media_tool(FFPROBE));
    hide_console_window(&mut command);
    let mut child = command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration,size,bit_rate,format_name:stream=index,codec_type,codec_name,width,height,avg_frame_rate,sample_rate,channels,bit_rate,start_time,duration,time_base",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| executable_error(FFPROBE, source))?;
    let stdout_worker = read_to_end(child.stdout.take().expect("piped stdout must be available"));
    let stderr_worker = drain_stderr(child.stderr.take().expect("piped stderr must be available"));
    let status = match wait_for_child(&mut child, &cancelled, path) {
        Ok(status) => status,
        Err(error) => {
            let _ = join_stdout(stdout_worker);
            let _ = join_stderr(stderr_worker);
            return Err(error);
        }
    };
    let stdout = join_stdout(stdout_worker).map_err(|source| WaveformError::Probe {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    let stderr = join_stderr(stderr_worker);
    if !status.success() {
        return Err(WaveformError::Probe {
            path: path.to_path_buf(),
            message: stderr_message(&stderr),
        });
    }
    let probe: FfprobeDocument =
        serde_json::from_slice(&stdout).map_err(|error| WaveformError::Probe {
            path: path.to_path_buf(),
            message: format!("invalid FFprobe metadata: {error}"),
        })?;
    Ok(media_metadata_from_probe(probe))
}

fn media_metadata_from_probe(probe: FfprobeDocument) -> MediaMetadata {
    let video = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"));
    let audio = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));
    let streams = probe
        .streams
        .iter()
        .enumerate()
        .map(|(fallback_index, stream)| MediaStreamMetadata {
            index: stream.index.unwrap_or(fallback_index),
            kind: stream.codec_type.clone(),
            codec: stream.codec_name.clone(),
            start_seconds: parse_nonnegative_f64(stream.start_time.as_deref()),
            duration_seconds: parse_positive_f64(stream.duration.as_deref()),
            time_base: stream.time_base.clone(),
            bit_rate: parse_positive_u64(stream.bit_rate.as_deref()),
            width: stream.width,
            height: stream.height,
            frame_rate: parse_frame_rate(stream.avg_frame_rate.as_deref()),
            frame_rate_ratio: parse_frame_rate_ratio(stream.avg_frame_rate.as_deref()),
            sample_rate: parse_positive_u64(stream.sample_rate.as_deref())
                .and_then(|value| value.try_into().ok()),
            channels: stream.channels,
        })
        .collect();
    MediaMetadata {
        duration_seconds: parse_positive_f64(probe.format.duration.as_deref()),
        file_size: parse_positive_u64(probe.format.size.as_deref()),
        container: probe.format.format_name,
        overall_bit_rate: parse_positive_u64(probe.format.bit_rate.as_deref()),
        video_codec: video.and_then(|stream| stream.codec_name.clone()),
        width: video.and_then(|stream| stream.width),
        height: video.and_then(|stream| stream.height),
        frame_rate: video.and_then(|stream| parse_frame_rate(stream.avg_frame_rate.as_deref())),
        frame_rate_ratio: video
            .and_then(|stream| parse_frame_rate_ratio(stream.avg_frame_rate.as_deref())),
        video_bit_rate: video.and_then(|stream| parse_positive_u64(stream.bit_rate.as_deref())),
        audio_codec: audio.and_then(|stream| stream.codec_name.clone()),
        sample_rate: audio.and_then(|stream| {
            parse_positive_u64(stream.sample_rate.as_deref())
                .and_then(|value| value.try_into().ok())
        }),
        channels: audio.and_then(|stream| stream.channels),
        audio_bit_rate: audio.and_then(|stream| parse_positive_u64(stream.bit_rate.as_deref())),
        streams,
    }
}

fn parse_positive_u64(value: Option<&str>) -> Option<u64> {
    value?.parse().ok().filter(|value| *value > 0)
}

fn parse_positive_f64(value: Option<&str>) -> Option<f64> {
    value?
        .parse()
        .ok()
        .filter(|value: &f64| value.is_finite() && *value > 0.0)
}

fn parse_nonnegative_f64(value: Option<&str>) -> Option<f64> {
    value?
        .parse()
        .ok()
        .filter(|value: &f64| value.is_finite() && *value >= 0.0)
}

fn parse_frame_rate(value: Option<&str>) -> Option<f64> {
    let ratio = parse_frame_rate_ratio(value)?;
    let rate = ratio.numerator as f64 / ratio.denominator as f64;
    if rate.is_finite() && rate > 0.0 {
        Some(rate)
    } else {
        None
    }
}

fn parse_frame_rate_ratio(value: Option<&str>) -> Option<FrameRate> {
    let (numerator, denominator) = value?.split_once('/')?;
    if numerator.is_empty()
        || denominator.is_empty()
        || !numerator.bytes().all(|byte| byte.is_ascii_digit())
        || !denominator.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let numerator = numerator.parse::<u64>().ok()?;
    let denominator = denominator.parse::<u64>().ok()?;
    if numerator == 0 || denominator == 0 {
        return None;
    }
    FrameRate::new(numerator, denominator)
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

#[derive(Debug)]
struct ProbeInfo {
    sample_rate: Option<u32>,
    channels: Option<usize>,
    duration_seconds: Option<f64>,
}

fn probe_audio(path: &Path, cancelled: &AtomicBool) -> Result<ProbeInfo, WaveformError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }
    let mut command = Command::new(media_tool(FFPROBE));
    hide_console_window(&mut command);
    let mut child = command
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=sample_rate,channels,duration:format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| executable_error(FFPROBE, source))?;
    let stdout_worker = read_to_end(child.stdout.take().expect("piped stdout must be available"));
    let stderr_worker = drain_stderr(child.stderr.take().expect("piped stderr must be available"));
    let status = match wait_for_child(&mut child, cancelled, path) {
        Ok(status) => status,
        Err(error) => {
            let _ = join_stdout(stdout_worker);
            let _ = join_stderr(stderr_worker);
            return Err(error);
        }
    };
    let stdout = join_stdout(stdout_worker).map_err(|source| WaveformError::Probe {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    let stderr = join_stderr(stderr_worker);
    if !status.success() {
        return Err(WaveformError::Probe {
            path: path.to_path_buf(),
            message: stderr_message(&stderr),
        });
    }

    let probe = parse_probe_output(&String::from_utf8_lossy(&stdout));
    if probe.sample_rate.is_none() || probe.channels.is_none() {
        return Err(WaveformError::NoAudio {
            path: path.to_path_buf(),
        });
    }
    Ok(probe)
}

fn parse_probe_output(output: &str) -> ProbeInfo {
    let mut sample_rate = None;
    let mut channels = None;
    let mut duration_seconds = None;
    for line in output.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key {
            "sample_rate" => sample_rate = value.parse().ok().filter(|rate: &u32| *rate != 0),
            "channels" => channels = value.parse().ok().filter(|count: &usize| *count != 0),
            "duration" => {
                duration_seconds = value
                    .parse::<f64>()
                    .ok()
                    .filter(|duration| duration.is_finite() && *duration >= 0.0)
            }
            _ => {}
        }
    }
    ProbeInfo {
        sample_rate,
        channels,
        duration_seconds,
    }
}

fn stream_decode(
    path: &Path,
    cancelled: &AtomicBool,
    mut on_sample: impl FnMut(f32),
) -> Result<u64, WaveformError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(WaveformError::Cancelled {
            path: path.to_path_buf(),
        });
    }
    let mut command = Command::new(media_tool(FFMPEG));
    hide_console_window(&mut command);
    let mut child = command
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-vn", "-ac", "1", "-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| executable_error(FFMPEG, source))?;
    let stdout = child.stdout.take().expect("piped stdout must be available");
    let stderr_worker = drain_stderr(child.stderr.take().expect("piped stderr must be available"));
    let (chunks, stdout_worker) = stream_stdout(stdout);
    let mut decoded_frames = 0_u64;
    let mut bytes = Vec::with_capacity(DECODE_BUFFER_BYTES + 3);

    loop {
        let chunk = match receive_chunk(&chunks, &mut child, cancelled, path) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                drop(chunks);
                let _ = join_stream_stdout(stdout_worker);
                let _ = join_stderr(stderr_worker);
                return Err(error);
            }
        };
        bytes.extend_from_slice(&chunk);
        let complete_bytes = bytes.len() / 4 * 4;
        for sample_bytes in bytes[..complete_bytes].chunks_exact(4) {
            let sample = f32::from_le_bytes(sample_bytes.try_into().expect("exact f32 bytes"));
            on_sample(normalize_sample(sample));
            decoded_frames = decoded_frames.saturating_add(1);
        }
        bytes.drain(..complete_bytes);
    }
    let status = match wait_for_child(&mut child, cancelled, path) {
        Ok(status) => status,
        Err(error) => {
            drop(chunks);
            let _ = join_stream_stdout(stdout_worker);
            let _ = join_stderr(stderr_worker);
            return Err(error);
        }
    };
    drop(chunks);
    if let Err(source) = join_stream_stdout(stdout_worker) {
        let stderr = join_stderr(stderr_worker);
        return Err(WaveformError::Decode {
            path: path.to_path_buf(),
            message: if stderr.is_empty() {
                source.to_string()
            } else {
                stderr_message(&stderr)
            },
        });
    }
    let stderr = join_stderr(stderr_worker);
    if !status.success() {
        return Err(WaveformError::Decode {
            path: path.to_path_buf(),
            message: stderr_message(&stderr),
        });
    }
    if !bytes.is_empty() {
        return Err(WaveformError::Decode {
            path: path.to_path_buf(),
            message: "FFmpeg emitted an incomplete f32le sample".to_owned(),
        });
    }
    Ok(decoded_frames)
}

fn normalize_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn executable_error(executable: &'static str, source: io::Error) -> WaveformError {
    WaveformError::ExecutableUnavailable { executable, source }
}

fn stderr_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();
    if message.is_empty() {
        "FFmpeg process failed without diagnostic output".to_owned()
    } else {
        message
    }
}

#[cfg(windows)]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console_window(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        process::{Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    fn temporary_path(name: &str) -> PathBuf {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("nle-waveform-{name}-{nanos}-{sequence}.wav"))
    }

    #[test]
    fn packaged_analysis_prefers_tool_beside_application_executable() {
        let directory = temporary_path("bundled-tool").with_extension("");
        fs::create_dir_all(&directory).unwrap();
        let application = directory.join(if cfg!(windows) {
            "Maelstrom.exe"
        } else {
            "Maelstrom"
        });
        let tool = directory.join(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        fs::write(&tool, b"test tool").unwrap();

        assert_eq!(
            media_tool_from_executable(FFPROBE, Some(&application)),
            tool
        );

        fs::remove_dir_all(directory).unwrap();
    }

    fn write_pcm_wav(path: &Path, samples: &[i16], channels: u16, sample_rate: u32) {
        let data_bytes = std::mem::size_of_val(samples) as u32;
        let block_align = channels * 2;
        let byte_rate = sample_rate * u32::from(block_align);
        let mut file = fs::File::create(path).expect("create test WAV");
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&channels.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&byte_rate.to_le_bytes()).unwrap();
        file.write_all(&block_align.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_bytes.to_le_bytes()).unwrap();
        for sample in samples {
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
    }

    fn ffmpeg_tools_available() -> bool {
        [FFMPEG, FFPROBE].into_iter().all(|tool| {
            Command::new(tool)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_reaps_an_active_streaming_child_promptly() {
        let mut command = Command::new("cmd");
        hide_console_window(&mut command);
        let mut child = command
            .args([
                "/C",
                "for /L %i in (1,1,60) do (@echo chunk & ping -n 2 127.0.0.1 >nul)",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start deliberately slow streaming child");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (chunks, stdout_worker) = stream_stdout(stdout);
        let stderr_worker = drain_stderr(stderr);
        let cancelled = Arc::new(AtomicBool::new(false));
        let path = Path::new("active-stream-test");

        // Receiving this chunk proves cancellation is not pre-spawn or
        // pre-stream; the process is actively producing stdout.
        assert!(
            receive_chunk(&chunks, &mut child, &cancelled, path)
                .expect("initial stream read")
                .is_some()
        );
        cancelled.store(true, Ordering::Release);
        let started = Instant::now();
        assert!(matches!(
            receive_chunk(&chunks, &mut child, &cancelled, path),
            Err(WaveformError::Cancelled { .. })
        ));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "cancellation should not wait for the child output interval"
        );
        drop(chunks);
        assert!(join_stream_stdout(stdout_worker).is_ok());
        let _ = join_stderr(stderr_worker);
        assert!(child.try_wait().expect("query reaped child").is_some());
    }

    #[test]
    fn probe_parser_uses_audio_stream_values() {
        let probe = parse_probe_output("sample_rate=48000\nchannels=2\nduration=2.000\n");
        assert_eq!(probe.sample_rate, Some(48_000));
        assert_eq!(probe.channels, Some(2));
        assert_eq!(probe.duration_seconds, Some(2.0));
    }

    #[test]
    fn probe_parser_rejects_missing_or_zero_values() {
        let probe = parse_probe_output("sample_rate=0\nchannels=N/A\n");
        assert_eq!(probe.sample_rate, None);
        assert_eq!(probe.channels, None);
        assert_eq!(probe.duration_seconds, None);
    }

    #[test]
    fn decoded_frame_timing_parser_is_named_and_rounds_to_decoder_microseconds() {
        assert_eq!(
            parse_frame_timestamp_line(
                b"pkt_duration_time=0.033367|best_effort_timestamp_time=0.033366667\n"
            ),
            Ok(Some(33_367))
        );
        assert_eq!(
            parse_frame_timestamp_line(b"best_effort_timestamp_time=0.033366\n"),
            Ok(Some(33_366))
        );
        assert!(parse_frame_timestamp_line(b"best_effort_timestamp_time=N/A\n").is_err());
        assert!(parse_frame_timestamp_line(b"pkt_duration_time=1\n").is_err());
    }

    #[test]
    fn frame_timing_probe_uses_bounded_decoded_best_effort_timestamps() {
        assert!(
            FRAME_TIMING_PROBE_ARGS
                .windows(2)
                .any(|pair| pair == ["-threads", "1"])
        );
        assert!(FRAME_TIMING_PROBE_ARGS.contains(&"-show_frames"));
        assert!(
            FRAME_TIMING_PROBE_ARGS
                .windows(2)
                .any(|pair| pair == ["-show_entries", "frame=best_effort_timestamp_time"])
        );
        assert!(!FRAME_TIMING_PROBE_ARGS.contains(&"-show_packets"));
    }

    #[test]
    fn decoded_frame_scan_handles_reordered_b_frame_vfr_media() {
        if !ffmpeg_tools_available() {
            return;
        }

        let path = temporary_path("vfr-b-frames").with_extension("mp4");
        let result = (|| -> Result<FrameTiming, String> {
            let status = Command::new(media_tool(FFMPEG))
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=32x32:rate=30:duration=1",
                    "-vf",
                    "select='eq(n,0)+eq(n,1)+eq(n,4)+eq(n,5)',setpts=PTS-STARTPTS",
                    "-fps_mode",
                    "vfr",
                    "-c:v",
                    "mpeg4",
                    "-bf",
                    "2",
                    "-q:v",
                    "2",
                    "-y",
                ])
                .arg(&path)
                .status()
                .map_err(|error| error.to_string())?;
            if !status.success() {
                return Err(format!("FFmpeg exited with {status}"));
            }
            analyze_frame_timing(&path).map_err(|error| error.to_string())
        })();
        let _ = fs::remove_file(&path);

        assert_eq!(
            result,
            Ok(FrameTiming::Variable(FrameTimeIndex {
                pts_microseconds: vec![0, 33_333, 133_333, 166_667],
            }))
        );
    }

    #[test]
    fn alternating_ntsc_microsecond_gaps_are_constant() {
        assert_eq!(
            classify_frame_timing(vec![0, 33_367, 66_733, 100_100, 133_467]),
            FrameTiming::Constant
        );
    }

    #[test]
    fn irregular_decoded_frame_gaps_publish_a_complete_variable_index() {
        let timing = classify_frame_timing(vec![100_000, 0, 33_333]);
        assert_eq!(
            timing,
            FrameTiming::Variable(FrameTimeIndex {
                pts_microseconds: vec![0, 33_333, 100_000],
            })
        );
        let FrameTiming::Variable(index) = timing else {
            panic!("expected variable timing");
        };
        assert_eq!(index.len(), 3);
        assert_eq!(index.pts(), &[0, 33_333, 100_000]);
        assert_eq!(index.into_pts(), vec![0, 33_333, 100_000]);
    }

    #[test]
    fn nonzero_origin_irregular_timestamps_publish_local_frame_index() {
        assert_eq!(
            classify_frame_timing(vec![41_667, 83_333, 166_667, 208_333, 291_667]),
            FrameTiming::Variable(FrameTimeIndex {
                pts_microseconds: vec![0, 41_666, 125_000, 166_666, 250_000],
            })
        );
    }

    #[test]
    fn negative_origin_irregular_timestamps_publish_local_frame_index() {
        assert_eq!(
            classify_frame_timing(vec![-41_667, 0, 83_333, 125_000]),
            FrameTiming::Variable(FrameTimeIndex {
                pts_microseconds: vec![0, 41_667, 125_000, 166_667],
            })
        );
    }

    #[test]
    fn supplied_reordered_vfr_fixture_publishes_local_presentation_timestamps() {
        let Some(path) = std::env::var_os("MAELSTROM_REORDERED_VFR_TEST_MEDIA") else {
            return;
        };
        let timing =
            analyze_frame_timing(PathBuf::from(path)).expect("scan supplied reordered VFR fixture");
        assert_eq!(
            timing,
            FrameTiming::Variable(FrameTimeIndex {
                pts_microseconds: vec![
                    0, 41_666, 125_000, 166_666, 250_000, 333_333, 458_333, 500_000,
                ],
            })
        );
    }

    #[test]
    fn duplicate_or_incomplete_decoded_frame_timestamps_are_unknown() {
        assert_eq!(
            classify_frame_timing(vec![0, 0, 33_333]),
            FrameTiming::Unknown
        );
        assert_eq!(classify_frame_timing(vec![0]), FrameTiming::Unknown);
    }

    #[test]
    fn timing_point_retention_stops_at_the_hard_cap() {
        let mut pts = vec![0; MAX_FRAME_TIMING_POINTS];
        assert!(retain_frame_timing_point(&mut pts, 1).is_err());
        assert_eq!(pts.len(), MAX_FRAME_TIMING_POINTS);
    }

    #[test]
    fn media_metadata_parser_selects_first_video_and_audio_streams() {
        let probe: FfprobeDocument = serde_json::from_str(
            r#"{
                "streams": [
                    {"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1088,"avg_frame_rate":"24/1","bit_rate":"28162692","start_time":"0.000000","duration":"15.041667","time_base":"1/12288"},
                    {"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"bit_rate":"128124","start_time":"0.000000","duration":"15.040000","time_base":"1/48000"}
                ],
                "format":{"format_name":"mov,mp4","duration":"15.041667","size":"53535107","bit_rate":"28472964"}
            }"#,
        )
        .unwrap();
        let metadata = media_metadata_from_probe(probe);
        assert_eq!(metadata.video_codec.as_deref(), Some("h264"));
        assert_eq!((metadata.width, metadata.height), (Some(1920), Some(1088)));
        assert_eq!(metadata.frame_rate, Some(24.0));
        assert_eq!(metadata.frame_rate_ratio, FrameRate::new(24, 1));
        assert_eq!(metadata.audio_codec.as_deref(), Some("aac"));
        assert_eq!(metadata.sample_rate, Some(48_000));
        assert_eq!(metadata.channels, Some(2));
        assert_eq!(metadata.file_size, Some(53_535_107));
        assert_eq!(metadata.streams.len(), 2);
        assert_eq!(metadata.streams[0].index, 0);
        assert_eq!(metadata.streams[0].time_base.as_deref(), Some("1/12288"));
        assert_eq!(metadata.streams[1].sample_rate, Some(48_000));
    }

    #[test]
    fn frame_rate_ratio_reduces_ntsc_rates_and_rejects_invalid_values() {
        assert_eq!(
            parse_frame_rate_ratio(Some("30000/1001")),
            FrameRate::new(30_000, 1_001)
        );
        assert_eq!(
            parse_frame_rate_ratio(Some("60000/2002")),
            FrameRate::new(30_000, 1_001)
        );
        assert_eq!(
            parse_frame_rate_ratio(Some("60000/1001")),
            FrameRate::new(60_000, 1_001)
        );
        for value in [
            None,
            Some(""),
            Some("0/1"),
            Some("1/0"),
            Some("1/-1"),
            Some("1/2/3"),
            Some("18446744073709551616/1"),
        ] {
            assert_eq!(parse_frame_rate_ratio(value), None, "{value:?}");
        }
    }

    #[test]
    fn externally_supplied_metadata_reports_every_stream() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let metadata = probe_media_metadata(PathBuf::from(path)).expect("probe supplied media");
        assert!(!metadata.streams.is_empty());
        assert!(
            metadata
                .streams
                .iter()
                .any(|stream| stream.kind.as_deref() == Some("video"))
        );
        assert!(metadata.streams.iter().all(|stream| stream.codec.is_some()));
    }

    #[test]
    fn generated_wav_has_bounded_non_empty_peaks_and_metadata_when_tools_available() {
        if !ffmpeg_tools_available() {
            return;
        }
        let path = temporary_path("peaks");
        write_pcm_wav(
            &path,
            &[-32_768, -8_000, 0, 8_000, 32_767, 0, -4_000, 4_000],
            1,
            8_000,
        );
        let waveform = analyze_path(&path, 4).expect("analyze generated WAV");
        fs::remove_file(&path).expect("clean test WAV");

        assert_eq!(waveform.peaks.len(), 4);
        assert_eq!(waveform.sample_rate, Some(8_000));
        assert_eq!(waveform.channels, Some(1));
        assert_eq!(waveform.total_frames, 8);
        assert_eq!(waveform.duration_seconds, Some(0.001));
        assert!(waveform.peaks.iter().all(|peak| {
            (-1.0..=1.0).contains(&peak.min)
                && (-1.0..=1.0).contains(&peak.max)
                && peak.min <= peak.max
        }));
        assert!(waveform.peaks.iter().any(|peak| peak.min < 0.0));
        assert!(waveform.peaks.iter().any(|peak| peak.max > 0.0));
    }

    #[test]
    fn missing_file_reports_io_error() {
        let path = temporary_path("missing");
        assert!(matches!(
            analyze_path(&path, 16),
            Err(WaveformError::Io { .. })
        ));
    }

    #[test]
    fn invalid_bin_count_is_rejected_before_file_io() {
        assert!(matches!(
            analyze_path("does-not-matter.wav", 0),
            Err(WaveformError::InvalidTargetBins { .. })
        ));
    }

    #[test]
    fn invalid_video_strip_request_is_rejected_before_file_io() {
        assert!(matches!(
            extract_video_strip("does-not-matter.mp4", 1.0, 0, 64),
            Err(WaveformError::InvalidVideoStrip { .. })
        ));
        assert!(matches!(
            extract_video_strip("does-not-matter.mp4", 1.0, MAX_VIDEO_STRIP_FRAMES + 1, 64),
            Err(WaveformError::InvalidVideoStrip { .. })
        ));
    }

    #[test]
    fn video_atlas_uses_compact_near_square_row_major_dimensions() {
        assert_eq!(video_atlas_dimensions(1), (1, 1));
        assert_eq!(video_atlas_dimensions(8), (3, 3));
        assert_eq!(video_atlas_dimensions(10), (4, 3));
        assert_eq!(video_atlas_dimensions(MAX_VIDEO_STRIP_FRAMES), (32, 32));
    }

    #[test]
    fn externally_supplied_media_decodes_when_requested() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let waveform = analyze_path(PathBuf::from(path), 512).expect("analyze supplied media");
        assert!(!waveform.peaks.is_empty());
        assert!(
            waveform
                .duration_seconds
                .is_some_and(|duration| duration > 0.0)
        );
        assert!(waveform.total_frames > 0);
    }

    #[test]
    fn externally_supplied_media_yields_a_bounded_video_strip_when_requested() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let strip = extract_video_strip(PathBuf::from(path), 15.0, 8, 72)
            .expect("extract strip from supplied media");
        assert_eq!(strip.duration_seconds, 15.0);
        assert_eq!(strip.frame_count, 8);
        assert_eq!(strip.frame_width, 128);
        assert_eq!(strip.frame_height, 72);
        assert_eq!((strip.columns, strip.rows), (3, 3));
        assert_eq!(strip.width, 3 * 128);
        assert_eq!(strip.height, 3 * 72);
        assert_eq!(
            strip.rgba.len(),
            strip.width as usize * strip.height as usize * 4
        );
    }
}
