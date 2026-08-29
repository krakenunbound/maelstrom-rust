//! Decode → RTX VSR → encode. Isolated from playback, same contract as nle-export.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crate::goal::dimensions;
use crate::rtx_vsr::{Frame, Quality, Session};

const CUT_THRESHOLD: f32 = 0.28;

#[derive(Clone, Debug)]
pub struct UpscaleRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub ffmpeg: PathBuf,
    pub quality: Quality,
    pub goal: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpscaleEvent {
    Progress(f32),
    Completed(PathBuf),
    Cancelled,
    Failed(String),
}

pub struct UpscaleJob {
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<UpscaleEvent>,
    join: Option<thread::JoinHandle<()>>,
}

impl UpscaleJob {
    pub fn start(
        request: UpscaleRequest,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        if !request.input.exists() {
            return Err(format!("source is missing: {}", request.input.display()));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, events) = mpsc::channel();
        let notify = Arc::new(notify);
        let join = thread::Builder::new()
            .name("maelstrom-kraken-upscale".into())
            .spawn(move || run_job(request, worker_cancel, tx, notify))
            .map_err(|error| format!("could not start Kraken Upscale: {error}"))?;
        Ok(Self {
            cancel,
            events,
            join: Some(join),
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn try_recv(&self) -> Result<UpscaleEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for UpscaleJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_job(
    request: UpscaleRequest,
    cancel: Arc<AtomicBool>,
    events: mpsc::Sender<UpscaleEvent>,
    notify: Arc<dyn Fn() + Send + Sync>,
) {
    let result = (|| -> Result<PathBuf, String> {
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        let probe = probe_video(&request.ffmpeg, &request.input)?;
        let (out_w, out_h) = dimensions(probe.width, probe.height, request.goal, true);
        if out_w <= probe.width && out_h <= probe.height {
            return Err(format!(
                "goal is not larger than the {}×{} source",
                probe.width, probe.height
            ));
        }
        crate::rtx_vsr::plan_output(probe.width, probe.height, out_w, out_h)?;
        upscale_video(&request, &probe, out_w, out_h, &cancel, &events, &notify)?;
        Ok(request.output.clone())
    })();
    let event = match result {
        Ok(path) if cancel.load(Ordering::Acquire) => UpscaleEvent::Cancelled,
        Ok(path) => UpscaleEvent::Completed(path),
        Err(error) if cancel.load(Ordering::Acquire) || error == "cancelled" => {
            UpscaleEvent::Cancelled
        }
        Err(error) => UpscaleEvent::Failed(error),
    };
    let _ = events.send(event);
    notify();
}

struct Probe {
    width: u32,
    height: u32,
    fps: f64,
    frames: u64,
}

fn probe_video(ffmpeg: &Path, input: &Path) -> Result<Probe, String> {
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let output = hidden_command(&ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate,r_frame_rate,nb_frames,duration",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=0",
            input.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|error| format!("could not run ffprobe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut width = 0u32;
    let mut height = 0u32;
    let mut fps = 0.0f64;
    let mut frames = 0u64;
    let mut duration = 0.0f64;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "width" => width = value.parse().unwrap_or(0),
            "height" => height = value.parse().unwrap_or(0),
            "avg_frame_rate" | "r_frame_rate" if fps <= 0.0 => fps = parse_rate(value),
            "nb_frames" => frames = value.parse().unwrap_or(0),
            "duration" => {
                if duration <= 0.0 {
                    duration = value.parse().unwrap_or(0.0);
                }
            }
            _ => {}
        }
    }
    if width == 0 || height == 0 {
        return Err("source has no video stream".into());
    }
    if fps <= 0.0 {
        fps = 30.0;
    }
    if frames == 0 && duration > 0.0 {
        frames = (duration * fps).round().max(1.0) as u64;
    }
    if frames == 0 {
        frames = 1;
    }
    Ok(Probe {
        width,
        height,
        fps,
        frames,
    })
}

fn parse_rate(value: &str) -> f64 {
    if let Some((n, d)) = value.split_once('/') {
        let n: f64 = n.parse().unwrap_or(0.0);
        let d: f64 = d.parse().unwrap_or(0.0);
        if d > 0.0 {
            return n / d;
        }
    }
    value.parse().unwrap_or(0.0)
}

fn upscale_video(
    request: &UpscaleRequest,
    probe: &Probe,
    out_w: u32,
    out_h: u32,
    cancel: &AtomicBool,
    events: &mpsc::Sender<UpscaleEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let mut decoder = hidden_command(&request.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            request.input.to_string_lossy().as_ref(),
            "-map",
            "0:v:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-an",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start decode ffmpeg: {error}"))?;
    let mut decoder_out = decoder
        .stdout
        .take()
        .ok_or_else(|| "missing decode pipe".to_owned())?;

    let fps = format_fps(probe.fps);
    let size = format!("{out_w}x{out_h}");
    let mut encoder = hidden_command(&request.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-s",
            &size,
            "-r",
            &fps,
            "-i",
            "pipe:0",
            "-i",
            request.input.to_string_lossy().as_ref(),
            "-map",
            "0:v:0",
            "-map",
            "1:a:0?",
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p5",
            "-cq",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-shortest",
            "-movflags",
            "+faststart",
            "-y",
            request.output.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            let _ = decoder.kill();
            format!("could not start encode ffmpeg: {error}")
        })?;
    let mut encoder_in = encoder
        .stdin
        .take()
        .ok_or_else(|| "missing encode pipe".to_owned())?;

    let mut session = Session::open(
        probe.width,
        probe.height,
        out_w,
        out_h,
        request.quality,
        true,
    )
    .map_err(|error| {
        teardown(&mut decoder, &mut encoder);
        error
    })?;

    let frame_bytes = probe.width as usize * probe.height as usize * 3;
    let mut rgb = vec![0u8; frame_bytes];
    let mut last_source: Option<Frame> = None;
    let mut done = 0u64;
    let total = probe.frames.max(1);

    let process = (|| -> Result<(), String> {
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            if !read_exact_or_eof(&mut decoder_out, &mut rgb)? {
                break;
            }
            let source = Frame {
                width: probe.width,
                height: probe.height,
                rgb: rgb.clone(),
            };
            if last_source
                .as_ref()
                .is_some_and(|previous| is_scene_cut(previous, &source))
            {
                session.reset_shot()?;
            }
            let output = session.enhance(&source)?;
            encoder_in
                .write_all(&output.rgb)
                .map_err(|error| format!("encode write failed: {error}"))?;
            last_source = Some(source);
            done += 1;
            let progress = (done as f32 / total as f32).clamp(0.0, 0.99);
            let _ = events.send(UpscaleEvent::Progress(progress));
            notify();
        }
        Ok(())
    })();

    drop(decoder_out);
    let _ = encoder_in.flush();
    drop(encoder_in);
    if cancel.load(Ordering::Acquire) {
        teardown(&mut decoder, &mut encoder);
    }
    let decoder_status = decoder.wait();
    let encoder_stderr = take_stderr(&mut encoder);
    let encoder_status = encoder.wait();

    process?;
    if cancel.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    if !decoder_status
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return Err("decode ffmpeg failed".into());
    }
    if !encoder_status
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return Err(format!("encode ffmpeg failed: {}", encoder_stderr.trim()));
    }
    let _ = events.send(UpscaleEvent::Progress(1.0));
    notify();
    Ok(())
}

fn teardown(decoder: &mut Child, encoder: &mut Child) {
    let _ = decoder.kill();
    let _ = encoder.kill();
}

fn take_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = BufReader::new(stderr).read_to_string(&mut text);
    }
    text
}

fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err("decode pipe ended on a partial frame".into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("decode read failed: {error}")),
        }
    }
    Ok(true)
}

fn is_scene_cut(left: &Frame, right: &Frame) -> bool {
    if left.rgb.len() != right.rgb.len() {
        return true;
    }
    let mut sum = 0.0f32;
    let mut samples = 0u64;
    for (index, (a, b)) in left
        .rgb
        .chunks_exact(3)
        .zip(right.rgb.chunks_exact(3))
        .enumerate()
    {
        if index % 16 != 0 {
            continue;
        }
        sum += (a[0] as f32 - b[0] as f32).abs()
            + (a[1] as f32 - b[1] as f32).abs()
            + (a[2] as f32 - b[2] as f32).abs();
        samples += 3;
    }
    samples > 0 && sum / (samples as f32 * 255.0) > CUT_THRESHOLD
}

fn format_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.001 {
        format!("{}", fps.round() as u32)
    } else {
        format!("{fps:.6}")
    }
}

fn hidden_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
