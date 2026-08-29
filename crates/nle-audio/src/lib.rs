//! Bounded in-process audio decode and native output for timeline transport.

use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

const FORWARD_REUSE_TICKS: i64 = 1_500_000;
/// The complete mix, rather than each lane, is limited to one second of PCM.
const MIX_BUFFER_SECONDS: usize = 1;
/// Hard per-decoder-frame guard; normal AAC/PCM frames are orders of magnitude smaller.
const MAX_DECODED_AUDIO_FRAME_SAMPLES: usize = 262_144;
/// Fixed callback-side acquisition attempts; this stays bounded and never parks the audio thread.
/// The spin hints give short decoder queue writes time to finish without risking an unbounded wait.
const CALLBACK_LOCK_TRY_ATTEMPTS: usize = 64;

/// Cumulative, non-blocking diagnostics from the native audio transport.
///
/// `output_callback_cpu_timing` aggregates elapsed CPU-side time spent in each
/// output callback. `callback_lock_failures` counts output callbacks that could not acquire the
/// mixer lock and therefore produced silence. `underrun_device_frames` counts
/// playing device frames for which every active lane was empty, including an
/// entire lock-contended callback. Paused callbacks never contribute to the
/// underrun-frame count. `late_decoded_frames_discarded` counts decoded stereo
/// frames dropped by the decode worker because the device clock had already
/// advanced past them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioRuntimeDiagnostics {
    pub output_callback_cpu_timing: AudioCallbackCpuTiming,
    pub callback_lock_failures: u64,
    pub underrun_device_frames: u64,
    pub late_decoded_frames_discarded: u64,
}

/// Cumulative CPU-side timing for native output callbacks, in nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioCallbackCpuTiming {
    pub samples: u64,
    pub total_nanos: u64,
    pub max_nanos: u64,
}

#[derive(Default)]
struct AudioRuntimeCounters {
    output_callback_cpu_sequence: AtomicU64,
    output_callback_cpu_samples: AtomicU64,
    output_callback_cpu_total_nanos: AtomicU64,
    output_callback_cpu_max_nanos: AtomicU64,
    callback_lock_failures: AtomicU64,
    underrun_device_frames: AtomicU64,
    late_decoded_frames_discarded: AtomicU64,
}

impl AudioRuntimeCounters {
    fn snapshot(&self) -> AudioRuntimeDiagnostics {
        AudioRuntimeDiagnostics {
            output_callback_cpu_timing: self.output_callback_cpu_timing_snapshot(),
            callback_lock_failures: self.callback_lock_failures.load(Ordering::Acquire),
            underrun_device_frames: self.underrun_device_frames.load(Ordering::Acquire),
            late_decoded_frames_discarded: self
                .late_decoded_frames_discarded
                .load(Ordering::Acquire),
        }
    }

    fn output_callback_cpu_timing_snapshot(&self) -> AudioCallbackCpuTiming {
        loop {
            let sequence_before = self.output_callback_cpu_sequence.load(Ordering::Acquire);
            if sequence_before % 2 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let timing = AudioCallbackCpuTiming {
                samples: self.output_callback_cpu_samples.load(Ordering::Relaxed),
                total_nanos: self.output_callback_cpu_total_nanos.load(Ordering::Relaxed),
                max_nanos: self.output_callback_cpu_max_nanos.load(Ordering::Relaxed),
            };
            let sequence_after = self.output_callback_cpu_sequence.load(Ordering::Acquire);
            if sequence_before == sequence_after {
                return timing;
            }
        }
    }

    fn record_output_callback_cpu_nanos(&self, nanos: u64) {
        // The CPAL output callback is this tuple's sole writer.
        self.output_callback_cpu_sequence
            .fetch_add(1, Ordering::AcqRel);
        let samples = self
            .output_callback_cpu_samples
            .load(Ordering::Relaxed)
            .saturating_add(1);
        let total_nanos = self
            .output_callback_cpu_total_nanos
            .load(Ordering::Relaxed)
            .saturating_add(nanos);
        let max_nanos = self
            .output_callback_cpu_max_nanos
            .load(Ordering::Relaxed)
            .max(nanos);
        self.output_callback_cpu_samples
            .store(samples, Ordering::Relaxed);
        self.output_callback_cpu_total_nanos
            .store(total_nanos, Ordering::Relaxed);
        self.output_callback_cpu_max_nanos
            .store(max_nanos, Ordering::Relaxed);
        self.output_callback_cpu_sequence
            .fetch_add(1, Ordering::Release);
    }

    fn record_callback_lock_failure(&self) {
        self.callback_lock_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn record_underrun_frames(&self, playing: bool, frames: usize) {
        if playing && frames > 0 {
            self.underrun_device_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
        }
    }

    fn record_late_discard(&self, frames: usize) {
        if frames > 0 {
            self.late_decoded_frames_discarded
                .fetch_add(frames as u64, Ordering::Relaxed);
        }
    }
}

/// The side of an equal-power audio transition that a clip supplies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioTransitionRole {
    Outgoing,
    Incoming,
}

/// A sample-accurate equal-power envelope applied after ordinary clip fades.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioTransitionEnvelope {
    pub role: AudioTransitionRole,
    pub start_clip_tick: i64,
    pub duration_ticks: i64,
}

/// A native real-time processor supported by the audio preview engine.
///
/// The timeline intentionally carries a broader effect catalog. Unsupported entries are
/// omitted by the app bridge rather than silently approximated during playback.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioProcessorSpec {
    HighPass { hz: u32 },
    LowPass { hz: u32 },
    Eq { hz: u32, db: f32 },
    StereoWidth { width: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioTarget {
    /// Timeline lane identity; enables independent decoder/session reuse.
    pub track_id: u32,
    pub clip_id: u32,
    pub path: PathBuf,
    pub source_tick: i64,
    pub clip_tick: i64,
    pub gain_db: f32,
    pub gain_left_db: f32,
    pub gain_right_db: f32,
    pub pan: f32,
    /// Ordered clip-then-track processors for this lane.
    pub effects: Vec<AudioProcessorSpec>,
    pub fade_in_ticks: i64,
    pub fade_in_curve: f32,
    pub fade_out_ticks: i64,
    pub fade_out_curve: f32,
    pub clip_duration_ticks: i64,
    pub transition: Option<AudioTransitionEnvelope>,
}

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn from_coefficients(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        let normalizer = a0
            .is_finite()
            .then_some(a0)
            .filter(|value| value.abs() > f32::EPSILON)
            .unwrap_or(1.0);
        Self {
            b0: b0 / normalizer,
            b1: b1 / normalizer,
            b2: b2 / normalizer,
            a1: a1 / normalizer,
            a2: a2 / normalizer,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn low_pass(hz: u32, sample_rate: u32) -> Self {
        let (cosine, alpha) = filter_terms(hz, sample_rate, std::f32::consts::FRAC_1_SQRT_2);
        Self::from_coefficients(
            (1.0 - cosine) * 0.5,
            1.0 - cosine,
            (1.0 - cosine) * 0.5,
            1.0 + alpha,
            -2.0 * cosine,
            1.0 - alpha,
        )
    }

    fn high_pass(hz: u32, sample_rate: u32) -> Self {
        let (cosine, alpha) = filter_terms(hz, sample_rate, std::f32::consts::FRAC_1_SQRT_2);
        Self::from_coefficients(
            (1.0 + cosine) * 0.5,
            -(1.0 + cosine),
            (1.0 + cosine) * 0.5,
            1.0 + alpha,
            -2.0 * cosine,
            1.0 - alpha,
        )
    }

    fn peaking_eq(hz: u32, db: f32, sample_rate: u32) -> Self {
        let (cosine, alpha) = filter_terms(hz, sample_rate, 1.0);
        let amplitude = 10.0_f32.powf(db.clamp(-60.0, 60.0) / 40.0);
        Self::from_coefficients(
            1.0 + alpha * amplitude,
            -2.0 * cosine,
            1.0 - alpha * amplitude,
            1.0 + alpha / amplitude,
            -2.0 * cosine,
            1.0 - alpha / amplitude,
        )
    }

    fn process(&mut self, input: f32) -> f32 {
        let input = input.is_finite().then_some(input).unwrap_or_default();
        let output = self.b0 * input + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = input;
        self.y2 = self.y1;
        self.y1 = output.is_finite().then_some(output).unwrap_or_default();
        self.y1
    }
}

fn filter_terms(hz: u32, sample_rate: u32, q: f32) -> (f32, f32) {
    let nyquist = sample_rate.max(2) as f32 * 0.5;
    // Keep the native processor aligned with the timeline/export 20 kHz
    // contract while retaining a final Nyquist guard for unusual devices.
    let frequency = (hz as f32).clamp(1.0, (nyquist * 0.98).min(20_000.0).max(1.0));
    let omega = std::f32::consts::TAU * frequency / sample_rate.max(2) as f32;
    let sine = omega.sin();
    (omega.cos(), sine / (2.0 * q.max(0.001)))
}

enum LaneProcessor {
    Biquad { left: Biquad, right: Biquad },
    StereoWidth { width: f32 },
}

impl LaneProcessor {
    fn from_spec(spec: &AudioProcessorSpec, sample_rate: u32) -> Self {
        match *spec {
            AudioProcessorSpec::HighPass { hz } => {
                let filter = Biquad::high_pass(hz, sample_rate);
                Self::Biquad {
                    left: filter,
                    right: filter,
                }
            }
            AudioProcessorSpec::LowPass { hz } => {
                let filter = Biquad::low_pass(hz, sample_rate);
                Self::Biquad {
                    left: filter,
                    right: filter,
                }
            }
            AudioProcessorSpec::Eq { hz, db } => {
                let filter = Biquad::peaking_eq(hz, db, sample_rate);
                Self::Biquad {
                    left: filter,
                    right: filter,
                }
            }
            AudioProcessorSpec::StereoWidth { width } => Self::StereoWidth {
                width: width
                    .is_finite()
                    .then_some(width.clamp(0.0, 2.0))
                    .unwrap_or(1.0),
            },
        }
    }

    fn process(&mut self, left: &mut f32, right: &mut f32) {
        match self {
            Self::Biquad {
                left: left_filter,
                right: right_filter,
            } => {
                *left = left_filter.process(*left);
                *right = right_filter.process(*right);
            }
            Self::StereoWidth { width } => {
                let mid = 0.5 * (*left + *right);
                let side = 0.5 * (*left - *right) * *width;
                *left = mid + side;
                *right = mid - side;
            }
        }
    }
}

fn build_processors(specs: &[AudioProcessorSpec], sample_rate: u32) -> Vec<LaneProcessor> {
    specs
        .iter()
        .map(|spec| LaneProcessor::from_spec(spec, sample_rate))
        .collect()
}

#[derive(Default)]
struct Shared {
    #[cfg(test)]
    // Test-only mirror retained for legacy decoder assertions.
    samples: VecDeque<f32>,
    lanes: HashMap<LaneKey, Lane>,
    generation: u64,
    device_frames: Arc<AtomicU64>,
    diagnostics: Arc<AudioRuntimeCounters>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct LaneKey {
    track_id: u32,
    clip_id: u32,
}

impl From<&AudioTarget> for LaneKey {
    fn from(target: &AudioTarget) -> Self {
        Self {
            track_id: target.track_id,
            clip_id: target.clip_id,
        }
    }
}

struct Lane {
    samples: VecDeque<f32>,
    /// Global device frame at which this lane joined the current transport.
    /// Decoding, fade timing, and stale PCM dropping are all relative to this origin.
    device_frame_origin: u64,
    decoded_frames: i64,
    target: Option<AudioTarget>,
    gain_left_linear: f32,
    gain_right_linear: f32,
    processors: Vec<LaneProcessor>,
}

impl Default for Lane {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            device_frame_origin: 0,
            decoded_frames: 0,
            target: None,
            gain_left_linear: 1.0,
            gain_right_linear: 1.0,
            processors: Vec::new(),
        }
    }
}

fn db_to_linear(gain_db: f32) -> f32 {
    10f32.powf(gain_db / 20.0)
}

fn channel_gains(target: &AudioTarget) -> (f32, f32) {
    let master = db_to_linear(target.gain_db);
    let pan = target.pan.clamp(-1.0, 1.0);
    (
        master * db_to_linear(target.gain_left_db) * (1.0 - pan).clamp(0.0, 1.0),
        master * db_to_linear(target.gain_right_db) * (1.0 + pan).clamp(0.0, 1.0),
    )
}

impl Shared {
    fn pop_stereo_frame(&mut self, playing: bool, sample_rate: u32) -> (f32, f32, bool) {
        if !playing {
            return (0.0, 0.0, false);
        }
        let device_frame = self.device_frames.load(Ordering::Acquire);
        let mut left = 0.0;
        let mut right = 0.0;
        let mut audible = false;
        for lane in self.lanes.values_mut() {
            if lane.samples.len() >= 2 {
                let fade = lane.target.as_ref().map_or(1.0, |target| {
                    let lane_frames = device_frame.saturating_sub(lane.device_frame_origin);
                    let tick = target.clip_tick.saturating_add(
                        lane_frames
                            .saturating_mul(1_000_000)
                            .checked_div(u64::from(sample_rate.max(1)))
                            .unwrap_or_default()
                            .min(i64::MAX as u64) as i64,
                    );
                    fade_envelope(tick, target) * transition_envelope(tick, target.transition)
                });
                let mut lane_left = lane.samples.pop_front().unwrap_or_default();
                let mut lane_right = lane.samples.pop_front().unwrap_or_default();
                for processor in &mut lane.processors {
                    processor.process(&mut lane_left, &mut lane_right);
                }
                left += lane_left * fade * lane.gain_left_linear;
                right += lane_right * fade * lane.gain_right_linear;
                audible = true;
            }
        }
        (
            left.is_finite()
                .then_some(left.clamp(-1.0, 1.0))
                .unwrap_or_default(),
            right
                .is_finite()
                .then_some(right.clamp(-1.0, 1.0))
                .unwrap_or_default(),
            audible,
        )
    }

    fn apply_target_mix_settings(lane: &mut Lane, target: &AudioTarget, sample_rate: u32) {
        let active = lane.target.as_mut().expect("active audio lane target");
        if active.effects != target.effects {
            lane.processors = build_processors(&target.effects, sample_rate);
            active.effects.clone_from(&target.effects);
        }
        active.gain_db = target.gain_db;
        active.gain_left_db = target.gain_left_db;
        active.gain_right_db = target.gain_right_db;
        active.pan = target.pan;
        active.fade_in_ticks = target.fade_in_ticks;
        active.fade_in_curve = target.fade_in_curve;
        active.fade_out_ticks = target.fade_out_ticks;
        active.fade_out_curve = target.fade_out_curve;
        active.clip_duration_ticks = target.clip_duration_ticks;
        active.transition = target.transition;
        let (left, right) = channel_gains(target);
        lane.gain_left_linear = left;
        lane.gain_right_linear = right;
    }

    fn update_mix_settings(&mut self, targets: &[AudioTarget], sample_rate: u32) -> bool {
        if targets.len() != self.lanes.len()
            || targets
                .iter()
                .any(|target| !self.lanes.contains_key(&LaneKey::from(target)))
        {
            return false;
        }
        for target in targets {
            let lane = self
                .lanes
                .get_mut(&LaneKey::from(target))
                .expect("validated active lane");
            Self::apply_target_mix_settings(lane, target, sample_rate);
        }
        true
    }

    /// Reconciles a changing set of concurrently audible timeline lanes without restarting the
    /// transport. Retained lanes preserve both decoded PCM and their original timeline origin.
    fn reconcile_targets(
        &mut self,
        targets: &[AudioTarget],
        sample_rate: u32,
    ) -> Option<(u64, HashSet<LaneKey>)> {
        if targets.is_empty() {
            return None;
        }
        let desired: HashSet<_> = targets.iter().map(LaneKey::from).collect();
        if desired.len() != targets.len() {
            return None;
        }
        for target in targets {
            let key = LaneKey::from(target);
            if let Some(lane) = self.lanes.get(&key) {
                let active = lane.target.as_ref()?;
                // Replacing the media for an existing identity cannot retain its queued PCM or
                // decoder safely. The caller must perform a real seek instead.
                if active.path != target.path {
                    return None;
                }
            }
        }

        let retained: HashSet<_> = desired
            .iter()
            .copied()
            .filter(|key| self.lanes.contains_key(key))
            .collect();
        // Cancel any in-flight job before removed lanes disappear. The next job resumes the
        // retained sessions, so this does not flush retained PCM or reset the device clock.
        self.generation = self.generation.wrapping_add(1);
        self.lanes.retain(|key, _| desired.contains(key));
        let device_frame_origin = self.device_frames.load(Ordering::Acquire);
        for target in targets {
            let key = LaneKey::from(target);
            if let Some(lane) = self.lanes.get_mut(&key) {
                Self::apply_target_mix_settings(lane, target, sample_rate);
            } else {
                let (gain_left_linear, gain_right_linear) = channel_gains(target);
                self.lanes.insert(
                    key,
                    Lane {
                        device_frame_origin,
                        target: Some(target.clone()),
                        gain_left_linear,
                        gain_right_linear,
                        processors: build_processors(&target.effects, sample_rate),
                        ..Lane::default()
                    },
                );
            }
        }
        Some((self.generation, retained))
    }
}

fn playback_source_tick(source_tick: i64, consumed_frames: u64, sample_rate: u32) -> Option<i64> {
    (consumed_frames > 0).then(|| {
        let elapsed = consumed_frames
            .saturating_mul(1_000_000)
            .checked_div(u64::from(sample_rate.max(1)))
            .unwrap_or_default()
            .min(i64::MAX as u64) as i64;
        source_tick.saturating_add(elapsed)
    })
}

fn advance_device_clock(device_frames: &AtomicU64, playing: bool, frames: usize) {
    if playing {
        device_frames.fetch_add(frames as u64, Ordering::AcqRel);
    }
}

fn try_lock_callback<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    for attempt in 0..CALLBACK_LOCK_TRY_ATTEMPTS {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::WouldBlock) => {
                if attempt + 1 < CALLBACK_LOCK_TRY_ATTEMPTS {
                    std::hint::spin_loop();
                }
            }
            Err(TryLockError::Poisoned(_)) => return None,
        }
    }
    None
}

fn stale_frames_to_skip(consumed_frames: u64, decoded_frame: i64) -> usize {
    (consumed_frames.min(i64::MAX as u64) as i64)
        .saturating_sub(decoded_frame)
        .max(0) as usize
}

fn sample_duration_ticks(samples: usize, sample_rate: u32) -> i64 {
    (samples.min(i64::MAX as usize) as i64).saturating_mul(1_000_000)
        / i64::from(sample_rate.max(1))
}

fn lane_capacity_samples(sample_rate: u32, lane_count: usize) -> usize {
    let total = (sample_rate as usize)
        .saturating_mul(2)
        .saturating_mul(MIX_BUFFER_SECONDS);
    // Stereo frames must never be split across queue boundaries.
    ((total / lane_count.max(1)) & !1).max(2)
}

fn enqueue_decoded_frame(
    state: &mut Shared,
    lane_key: LaneKey,
    floats: &[f32],
    sample_rate: u32,
) -> Result<bool, String> {
    let lane_count = state.lanes.len();
    let diagnostics = Arc::clone(&state.diagnostics);
    let lane = state.lanes.get_mut(&lane_key).expect("active audio lane");
    let consumed_frames = state
        .device_frames
        .load(Ordering::Acquire)
        .saturating_sub(lane.device_frame_origin);
    let frame_base = lane.decoded_frames;
    let stale_frames = stale_frames_to_skip(consumed_frames, frame_base);
    let stale_samples = stale_frames.saturating_mul(2).min(floats.len()) & !1;
    let capacity = lane_capacity_samples(sample_rate, lane_count);
    let hard_capacity = capacity.saturating_add(MAX_DECODED_AUDIO_FRAME_SAMPLES);
    if lane
        .samples
        .len()
        .saturating_add(floats.len().saturating_sub(stale_samples))
        > hard_capacity
    {
        return Err("decoded audio packet exceeds the bounded lane allowance".to_owned());
    }
    diagnostics.record_late_discard(stale_samples / 2);
    lane.samples.extend(floats[stale_samples..].iter().copied());
    lane.decoded_frames = frame_base.saturating_add((floats.len() / 2) as i64);
    let reached_capacity = lane.samples.len() >= capacity;
    #[cfg(test)]
    state
        .samples
        .extend(floats[stale_samples..].iter().copied());
    Ok(reached_capacity)
}

fn resolved_channel_layout(layout: ffmpeg::ChannelLayout, channels: u16) -> ffmpeg::ChannelLayout {
    if layout.is_empty() {
        ffmpeg::ChannelLayout::default(i32::from(channels.max(1)))
    } else {
        layout
    }
}

struct DecodeJob {
    generation: u64,
    targets: Vec<AudioTarget>,
    /// Existing lanes whose sticky decoders and queued PCM remain valid.
    resume_lanes: HashSet<LaneKey>,
}

#[derive(Default)]
struct SchedulerState {
    pending: Option<DecodeJob>,
    shutdown: bool,
}

impl SchedulerState {
    fn submit(&mut self, generation: u64, targets: Vec<AudioTarget>) {
        self.submit_reconcile(generation, targets, HashSet::new());
    }

    fn submit_reconcile(
        &mut self,
        generation: u64,
        targets: Vec<AudioTarget>,
        resume_lanes: HashSet<LaneKey>,
    ) {
        self.pending = Some(DecodeJob {
            generation,
            targets,
            resume_lanes,
        });
    }
}

#[derive(Default)]
struct AudioMeter {
    left: AtomicU32,
    right: AtomicU32,
}

impl AudioMeter {
    fn store(&self, left: f32, right: f32) {
        self.left
            .store(meter_value(left).to_bits(), Ordering::Release);
        self.right
            .store(meter_value(right).to_bits(), Ordering::Release);
    }

    fn load(&self) -> (f32, f32) {
        (
            f32::from_bits(self.left.load(Ordering::Acquire)),
            f32::from_bits(self.right.load(Ordering::Acquire)),
        )
    }

    fn clear(&self) {
        self.store(0.0, 0.0);
    }
}

fn meter_value(value: f32) -> f32 {
    if value.is_finite() {
        value.abs().clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Owns a native output stream and a bounded, throw-away PCM queue. This is a
/// transport buffer, not a timeline/media cache: every seek replaces it.
pub struct AudioEngine {
    shared: Arc<Mutex<Shared>>,
    device_frames: Arc<AtomicU64>,
    source_tick: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<String>>>,
    meter: Arc<AudioMeter>,
    diagnostics: Arc<AudioRuntimeCounters>,
    scheduler: Arc<(Mutex<SchedulerState>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    sample_rate: u32,
    _stream: cpal::Stream,
}

struct OutputResources {
    shared: Arc<Mutex<Shared>>,
    playing: Arc<AtomicBool>,
    device_frames: Arc<AtomicU64>,
    errors: Arc<Mutex<Option<String>>>,
    meter: Arc<AudioMeter>,
    diagnostics: Arc<AudioRuntimeCounters>,
}

impl AudioEngine {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no default audio output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let config = supported.config();
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate.0;
        let device_frames = Arc::new(AtomicU64::new(0));
        let source_tick = Arc::new(AtomicI64::new(0));
        let diagnostics = Arc::new(AudioRuntimeCounters::default());
        let shared = Arc::new(Mutex::new(Shared {
            device_frames: Arc::clone(&device_frames),
            diagnostics: Arc::clone(&diagnostics),
            ..Shared::default()
        }));
        let playing = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(None));
        let meter = Arc::new(AudioMeter::default());
        let output = OutputResources {
            shared: Arc::clone(&shared),
            playing: Arc::clone(&playing),
            device_frames: Arc::clone(&device_frames),
            errors: Arc::clone(&errors),
            meter: Arc::clone(&meter),
            diagnostics: Arc::clone(&diagnostics),
        };
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                build_output_stream::<f32>(&device, &config, channels, &output)
            }
            cpal::SampleFormat::I16 => {
                build_output_stream::<i16>(&device, &config, channels, &output)
            }
            cpal::SampleFormat::U16 => {
                build_output_stream::<u16>(&device, &config, channels, &output)
            }
            format => {
                return Err(format!(
                    "unsupported audio output sample format: {format:?}"
                ));
            }
        }?;
        stream.play().map_err(|e| e.to_string())?;
        let scheduler = Arc::new((Mutex::new(SchedulerState::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let worker_errors = Arc::clone(&errors);
        let worker_scheduler = Arc::clone(&scheduler);
        let worker = thread::Builder::new()
            .name("maelstrom-audio-decode".to_owned())
            .spawn(move || {
                audio_decode_worker(worker_shared, worker_errors, worker_scheduler, sample_rate)
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            shared,
            device_frames,
            source_tick,
            playing,
            errors,
            meter,
            diagnostics,
            scheduler,
            worker: Some(worker),
            sample_rate,
            _stream: stream,
        })
    }

    pub fn pause(&self) {
        self.playing.store(false, Ordering::Release);
        let mut state = self.shared.lock().expect("audio state lock");
        state.generation = state.generation.wrapping_add(1);
        state.lanes.clear();
        drop(state);
        self.device_frames.store(0, Ordering::Release);
        self.meter.clear();
        let (scheduler, wake) = &*self.scheduler;
        scheduler.lock().expect("audio scheduler lock").pending = None;
        wake.notify_one();
    }

    pub fn seek_and_play(&self, target: AudioTarget) {
        self.seek_and_play_all(vec![target]);
    }

    /// Starts every currently audible timeline lane on the same device clock.
    pub fn seek_and_play_all(&self, targets: Vec<AudioTarget>) {
        if targets.is_empty() {
            self.pause();
            return;
        }
        // Freeze the device clock while replacing every lane so no callback frame can be
        // counted against the new source origin before its queues and decode job exist.
        self.playing.store(false, Ordering::Release);
        self.source_tick
            .store(targets[0].source_tick, Ordering::Release);
        self.device_frames.store(0, Ordering::Release);
        let generation = {
            let mut state = self.shared.lock().expect("audio state lock");
            state.generation = state.generation.wrapping_add(1);
            state.lanes = targets
                .iter()
                .map(|target| {
                    let (gain_left_linear, gain_right_linear) = channel_gains(target);
                    (
                        LaneKey::from(target),
                        Lane {
                            target: Some(target.clone()),
                            gain_left_linear,
                            gain_right_linear,
                            processors: build_processors(&target.effects, self.sample_rate),
                            ..Lane::default()
                        },
                    )
                })
                .collect();
            state.generation
        };
        self.meter.clear();
        let (scheduler, wake) = &*self.scheduler;
        scheduler
            .lock()
            .expect("audio scheduler lock")
            .submit(generation, targets);
        self.playing.store(true, Ordering::Release);
        wake.notify_one();
    }

    /// Updates gain and fades on the live mixer without discarding decoded PCM or restarting
    /// the decoder. Returns false when the requested lanes are not the active transport.
    pub fn update_mix_settings(&self, targets: &[AudioTarget]) -> bool {
        let mut state = self.shared.lock().expect("audio state lock");
        state.update_mix_settings(targets, self.sample_rate)
    }

    /// Adds and removes the currently audible timeline lanes without interrupting playback.
    ///
    /// Retained lanes keep their PCM queues, device-frame origin, and sticky decoder session.
    /// New lanes start at their supplied source and clip tick at the current device frame. A
    /// false result means the request would require replacing retained media or is otherwise not
    /// safe to reconcile; callers should use `seek_and_play_all` for that case.
    pub fn reconcile_playing_targets(&self, targets: Vec<AudioTarget>) -> bool {
        if !self.playing.load(Ordering::Acquire) {
            return false;
        }
        let (generation, resume_lanes) = {
            let mut state = self.shared.lock().expect("audio state lock");
            let Some((generation, resume_lanes)) =
                state.reconcile_targets(&targets, self.sample_rate)
            else {
                return false;
            };
            (generation, resume_lanes)
        };
        let (scheduler, wake) = &*self.scheduler;
        scheduler
            .lock()
            .expect("audio scheduler lock")
            .submit_reconcile(generation, targets, resume_lanes);
        wake.notify_one();
        true
    }

    pub fn take_error(&self) -> Option<String> {
        self.errors.lock().expect("audio error lock").take()
    }

    /// Peak levels from the samples actually consumed by the output callback.
    pub fn meter_levels(&self) -> (f32, f32) {
        self.meter.load()
    }

    /// Returns cumulative callback and decode-worker diagnostics without blocking.
    pub fn runtime_diagnostics(&self) -> AudioRuntimeDiagnostics {
        self.diagnostics.snapshot()
    }

    /// Source-media position of samples actually consumed by the native device callback.
    /// Queue underruns advance as device silence; late decoded PCM is discarded to this clock.
    pub fn playback_source_tick(&self) -> Option<i64> {
        if !self.playing.load(Ordering::Acquire) {
            return None;
        }
        playback_source_tick(
            self.source_tick.load(Ordering::Acquire),
            self.device_frames.load(Ordering::Acquire),
            self.sample_rate,
        )
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.pause();
        let (scheduler, wake) = &*self.scheduler;
        scheduler.lock().expect("audio scheduler lock").shutdown = true;
        wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn audio_decode_worker(
    shared: Arc<Mutex<Shared>>,
    errors: Arc<Mutex<Option<String>>>,
    scheduler: Arc<(Mutex<SchedulerState>, Condvar)>,
    sample_rate: u32,
) {
    // A lane owns one active decoder. This allows transition overlaps from two clips on the
    // same track while retaining sticky reuse within an unchanged clip lane.
    let mut sessions: HashMap<LaneKey, StickyAudio> = HashMap::new();
    loop {
        let job = {
            let (state, wake) = &*scheduler;
            let mut state = state.lock().expect("audio scheduler lock");
            while state.pending.is_none() && !state.shutdown {
                state = wake.wait(state).expect("audio scheduler lock");
            }
            if state.shutdown {
                return;
            }
            state.pending.take().expect("pending audio job")
        };
        let active_lanes: HashSet<_> = job.targets.iter().map(LaneKey::from).collect();
        sessions.retain(|key, _| active_lanes.contains(key));
        let mut resumed_lanes = job.resume_lanes;
        let mut eof_lanes = HashSet::new();
        loop {
            for target in &job.targets {
                let key = LaneKey::from(target);
                if eof_lanes.contains(&key) {
                    continue;
                }
                let needs_open = sessions.get(&key).is_none_or(|active| {
                    active.path != target.path || active.sample_rate != sample_rate
                });
                if needs_open {
                    match StickyAudio::open(target, sample_rate) {
                        Ok(session) => {
                            sessions.insert(key, session);
                        }
                        Err(error) => {
                            resumed_lanes.remove(&key);
                            eof_lanes.insert(key);
                            if shared.lock().expect("audio state lock").generation == job.generation
                            {
                                *errors.lock().expect("audio error lock") = Some(error);
                            }
                            continue;
                        }
                    }
                }
                // A reconciliation can overtake the initial decode job. Only an actually
                // retained decoder may skip `prepare`; a newly opened session must seek to its
                // supplied source tick.
                let resume = !needs_open && resumed_lanes.contains(&key);
                let result = sessions
                    .get_mut(&key)
                    .expect("audio session opened")
                    .decode_into_queue(Arc::clone(&shared), target, key, resume, |state| {
                        state.generation != job.generation
                    });
                resumed_lanes.insert(key);
                if let Err(error) = result {
                    sessions.remove(&key);
                    resumed_lanes.remove(&key);
                    eof_lanes.insert(key);
                    if shared.lock().expect("audio state lock").generation == job.generation {
                        *errors.lock().expect("audio error lock") = Some(error);
                    }
                } else if sessions.get(&key).is_some_and(|session| session.eof) {
                    eof_lanes.insert(key);
                }
            }
            let still_current =
                shared.lock().expect("audio state lock").generation == job.generation;
            if !still_current {
                break;
            }
            let (state, wake) = &*scheduler;
            if state
                .lock()
                .expect("audio scheduler lock")
                .pending
                .is_some()
            {
                break;
            }
            if eof_lanes.len() == job.targets.len() {
                let mut state = state.lock().expect("audio scheduler lock");
                while state.pending.is_none() && !state.shutdown {
                    state = wake.wait(state).expect("audio scheduler lock");
                }
                if state.shutdown {
                    return;
                }
                break;
            }
            thread::sleep(std::time::Duration::from_millis(4));
        }
    }
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    resources: &OutputResources,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let callback_shared = Arc::clone(&resources.shared);
    let callback_playing = Arc::clone(&resources.playing);
    let callback_device_frames = Arc::clone(&resources.device_frames);
    let callback_errors = Arc::clone(&resources.errors);
    let callback_meter = Arc::clone(&resources.meter);
    let callback_diagnostics = Arc::clone(&resources.diagnostics);
    let sample_rate = config.sample_rate.0;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let callback_start = Instant::now();
                let is_playing = callback_playing.load(Ordering::Acquire);
                let Some(mut state) = try_lock_callback(&callback_shared) else {
                    callback_diagnostics.record_callback_lock_failure();
                    let frames = data.len() / channels.max(1);
                    callback_diagnostics.record_underrun_frames(is_playing, frames);
                    advance_device_clock(&callback_device_frames, is_playing, frames);
                    callback_meter.clear();
                    for sample in data.iter_mut() {
                        *sample = T::from_sample(0.0);
                    }
                    callback_diagnostics.record_output_callback_cpu_nanos(
                        callback_start
                            .elapsed()
                            .as_nanos()
                            .try_into()
                            .unwrap_or(u64::MAX),
                    );
                    return;
                };
                let mut peak_left = 0.0_f32;
                let mut peak_right = 0.0_f32;
                let mut underrun_frames = 0_usize;
                for frame in data.chunks_mut(channels) {
                    let (left, right, audible) = state.pop_stereo_frame(is_playing, sample_rate);
                    if !audible {
                        underrun_frames += 1;
                    }
                    peak_left = peak_left.max(left.abs());
                    peak_right = peak_right.max(right.abs());
                    for (channel, sample) in frame.iter_mut().enumerate() {
                        let value = match channel {
                            0 => left,
                            1 => right,
                            _ => 0.5 * (left + right),
                        };
                        *sample = T::from_sample(value);
                    }
                    // Advance immediately after this frame so fades and transition envelopes
                    // observe a distinct timeline tick for every device sample.
                    advance_device_clock(&callback_device_frames, is_playing, 1);
                }
                callback_diagnostics.record_underrun_frames(is_playing, underrun_frames);
                callback_meter.store(peak_left, peak_right);
                callback_diagnostics.record_output_callback_cpu_nanos(
                    callback_start
                        .elapsed()
                        .as_nanos()
                        .try_into()
                        .unwrap_or(u64::MAX),
                );
            },
            move |error| {
                *callback_errors.lock().expect("audio error lock") = Some(error.to_string());
            },
            None,
        )
        .map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionAction {
    Open,
    Continue,
    SeekAndFlush,
}

fn session_action(
    active_path: Option<&Path>,
    active_sample_rate: Option<u32>,
    decoder_cursor_tick: Option<i64>,
    target: &AudioTarget,
    sample_rate: u32,
) -> SessionAction {
    if active_path != Some(target.path.as_path()) || active_sample_rate != Some(sample_rate) {
        return SessionAction::Open;
    }
    if decoder_cursor_tick.is_some_and(|last| {
        target.source_tick > last && target.source_tick.saturating_sub(last) <= FORWARD_REUSE_TICKS
    }) {
        SessionAction::Continue
    } else {
        SessionAction::SeekAndFlush
    }
}

struct StickyAudio {
    path: PathBuf,
    sample_rate: u32,
    input: ffmpeg::format::context::Input,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    decoder: ffmpeg::decoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
    decoder_cursor_tick: Option<i64>,
    eof: bool,
    #[cfg(test)]
    seek_count: usize,
}

impl StickyAudio {
    fn open(target: &AudioTarget, sample_rate: u32) -> Result<Self, String> {
        ffmpeg::init().map_err(|error| error.to_string())?;
        let input = ffmpeg::format::input(&target.path).map_err(|error| error.to_string())?;
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Audio)
            .ok_or("no audio stream")?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| e.to_string())?;
        let decoder = context.decoder().audio().map_err(|e| e.to_string())?;
        let resampler = Self::make_resampler(
            decoder.format(),
            resolved_channel_layout(decoder.channel_layout(), decoder.channels()),
            decoder.rate(),
            sample_rate,
        )?;
        Ok(Self {
            path: target.path.clone(),
            sample_rate,
            input,
            stream_index,
            time_base,
            decoder,
            resampler,
            decoder_cursor_tick: None,
            eof: false,
            #[cfg(test)]
            seek_count: 0,
        })
    }

    fn make_resampler(
        format: ffmpeg::format::Sample,
        layout: ffmpeg::ChannelLayout,
        rate: u32,
        sample_rate: u32,
    ) -> Result<ffmpeg::software::resampling::Context, String> {
        ffmpeg::software::resampling::Context::get(
            format,
            layout,
            rate,
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            ffmpeg::ChannelLayout::STEREO,
            sample_rate,
        )
        .map_err(|error| error.to_string())
    }

    fn prepare(&mut self, target: &AudioTarget) -> Result<SessionAction, String> {
        let action = session_action(
            Some(&self.path),
            Some(self.sample_rate),
            self.decoder_cursor_tick,
            target,
            self.sample_rate,
        );
        debug_assert_ne!(action, SessionAction::Open);
        if action == SessionAction::SeekAndFlush {
            let target_ts = target
                .source_tick
                .max(0)
                .rescale((1, 1_000_000), self.time_base);
            self.input
                .seek(target_ts, ..target_ts)
                .map_err(|error| error.to_string())?;
            self.decoder.flush();
            // A discontinuous seek discards delayed resampler output. Recreate only this cheap
            // conversion state while retaining the open demuxer and decoder session.
            self.resampler = Self::make_resampler(
                self.decoder.format(),
                resolved_channel_layout(self.decoder.channel_layout(), self.decoder.channels()),
                self.decoder.rate(),
                self.sample_rate,
            )?;
            self.decoder_cursor_tick = None;
            self.eof = false;
            #[cfg(test)]
            {
                self.seek_count += 1;
            }
        }
        Ok(action)
    }

    fn decode_into_queue(
        &mut self,
        shared: Arc<Mutex<Shared>>,
        target: &AudioTarget,
        lane_key: LaneKey,
        resume: bool,
        is_cancelled: impl Fn(&Shared) -> bool,
    ) -> Result<SessionAction, String> {
        let action = if resume {
            SessionAction::Continue
        } else {
            self.prepare(target)?
        };
        {
            let state = shared.lock().expect("audio state lock");
            if is_cancelled(&state) {
                return Ok(action);
            }
            let capacity = lane_capacity_samples(self.sample_rate, state.lanes.len());
            if state
                .lanes
                .get(&lane_key)
                .is_none_or(|lane| lane.samples.len() >= capacity)
            {
                return Ok(action);
            }
        }
        let stream_index = self.stream_index;
        let time_base = self.time_base;
        let decoder = &mut self.decoder;
        let resampler = &mut self.resampler;
        let decoder_cursor_tick = &mut self.decoder_cursor_tick;
        let eof = &mut self.eof;
        for (stream, packet) in self.input.packets() {
            if stream.index() != stream_index {
                let state = shared.lock().expect("audio state lock");
                if is_cancelled(&state) {
                    return Ok(action);
                }
                continue;
            }
            decoder.send_packet(&packet).map_err(|e| e.to_string())?;
            let mut decoded = ffmpeg::frame::Audio::empty();
            let mut cancelled = false;
            let mut reached_capacity = false;
            while decoder.receive_frame(&mut decoded).is_ok() {
                if decoded.channel_layout().is_empty() {
                    decoded.set_channel_layout(resolved_channel_layout(
                        decoded.channel_layout(),
                        decoded.channels(),
                    ));
                }
                let decoded_start_tick = decoded
                    .timestamp()
                    .or_else(|| decoded.pts())
                    .map(|timestamp| timestamp.rescale(time_base, (1, 1_000_000)))
                    .unwrap_or(target.source_tick);
                // Drain every frame produced by the accepted packet before returning on
                // cancellation. Otherwise a later continuation can skip decoder-buffered PCM.
                *decoder_cursor_tick = Some(
                    decoded_start_tick
                        .saturating_add(sample_duration_ticks(decoded.samples(), decoded.rate())),
                );
                if cancelled || {
                    let state = shared.lock().expect("audio state lock");
                    is_cancelled(&state)
                } {
                    cancelled = true;
                    continue;
                }
                if resampler.input().format != decoded.format()
                    || resampler.input().channel_layout != decoded.channel_layout()
                    || resampler.input().rate != decoded.rate()
                {
                    *resampler = Self::make_resampler(
                        decoded.format(),
                        decoded.channel_layout(),
                        decoded.rate(),
                        self.sample_rate,
                    )?;
                }
                let mut output = ffmpeg::frame::Audio::empty();
                let mut output_start_tick = decoded_start_tick.saturating_sub(
                    resampler
                        .delay()
                        .map(|delay| {
                            sample_duration_ticks(delay.input.max(0) as usize, decoded.rate())
                        })
                        .unwrap_or_default(),
                );
                match resampler.run(&decoded, &mut output) {
                    Ok(_) => {}
                    Err(error @ (ffmpeg::Error::InputChanged | ffmpeg::Error::OutputChanged)) => {
                        *resampler = Self::make_resampler(
                            decoded.format(),
                            decoded.channel_layout(),
                            decoded.rate(),
                            self.sample_rate,
                        )?;
                        output = ffmpeg::frame::Audio::empty();
                        output_start_tick = decoded_start_tick;
                        resampler
                            .run(&decoded, &mut output)
                            .map_err(|retry_error| {
                                format!(
                                    "resampler changed ({error}); retry failed: {retry_error}; decoded format={:?} layout={:?} rate={}",
                                    decoded.format(),
                                    decoded.channel_layout(),
                                    decoded.rate()
                                )
                            })?;
                    }
                    Err(error) => return Err(error.to_string()),
                }
                let pcm = output.data(0);
                let valid_samples = output.samples().saturating_mul(output.channels() as usize);
                if valid_samples > MAX_DECODED_AUDIO_FRAME_SAMPLES {
                    return Err(format!(
                        "decoded audio frame has {valid_samples} samples; maximum is {MAX_DECODED_AUDIO_FRAME_SAMPLES}"
                    ));
                }
                let all_floats: &[f32] = unsafe {
                    std::slice::from_raw_parts(
                        pcm.as_ptr().cast(),
                        valid_samples.min(pcm.len() / 4),
                    )
                };
                // FFmpeg seeks to a packet/keyframe at or before the requested time. Discard the
                // preroll portion so audible output begins at the timeline cursor, not earlier.
                let skip_frames = target
                    .source_tick
                    .saturating_sub(output_start_tick)
                    .max(0)
                    .saturating_mul(i64::from(self.sample_rate))
                    / 1_000_000;
                let skip_samples = (skip_frames as usize)
                    .saturating_mul(2)
                    .min(all_floats.len());
                let floats = &all_floats[skip_samples..];
                let mut state = shared.lock().expect("audio state lock");
                if is_cancelled(&state) {
                    cancelled = true;
                    continue;
                }
                reached_capacity |=
                    enqueue_decoded_frame(&mut state, lane_key, floats, self.sample_rate)?;
            }
            if cancelled {
                // Drained frames were deliberately not resampled. Drop any delayed conversion
                // state so a future sequential request begins exactly at decoder_cursor_tick.
                *resampler = Self::make_resampler(
                    decoder.format(),
                    resolved_channel_layout(decoder.channel_layout(), decoder.channels()),
                    decoder.rate(),
                    self.sample_rate,
                )?;
                return Ok(action);
            }
            if reached_capacity {
                // Stop only at a packet boundary so decoder-buffered PCM is never discarded.
                // The next fair worker round resumes this same open demuxer/decoder session.
                return Ok(action);
            }
        }
        *eof = true;
        Ok(action)
    }
}

#[cfg(test)]
fn decode_into_queue(
    shared: Arc<Mutex<Shared>>,
    generation: u64,
    target: AudioTarget,
    sample_rate: u32,
) -> Result<(), String> {
    shared
        .lock()
        .expect("audio state lock")
        .lanes
        .entry(LaneKey::from(&target))
        .or_default();
    StickyAudio::open(&target, sample_rate)?
        .decode_into_queue(shared, &target, LaneKey::from(&target), false, |state| {
            state.generation != generation
        })
        .map(|_| ())
}

fn fade_envelope(clip_tick: i64, target: &AudioTarget) -> f32 {
    let fade_in = if target.fade_in_ticks > 0 {
        shaped_fade(
            (clip_tick as f32 / target.fade_in_ticks as f32).clamp(0.0, 1.0),
            target.fade_in_curve,
        )
    } else {
        1.0
    };
    let remaining = target.clip_duration_ticks.saturating_sub(clip_tick);
    let fade_out = if target.fade_out_ticks > 0 {
        shaped_fade(
            (remaining as f32 / target.fade_out_ticks as f32).clamp(0.0, 1.0),
            target.fade_out_curve,
        )
    } else {
        1.0
    };
    fade_in.min(fade_out)
}

fn transition_envelope(clip_tick: i64, transition: Option<AudioTransitionEnvelope>) -> f32 {
    let Some(transition) = transition else {
        return 1.0;
    };
    if transition.duration_ticks <= 0 {
        return 1.0;
    }
    let progress = (clip_tick.saturating_sub(transition.start_clip_tick) as f64
        / transition.duration_ticks as f64)
        .clamp(0.0, 1.0);
    if progress == 0.0 {
        return match transition.role {
            AudioTransitionRole::Outgoing => 1.0,
            AudioTransitionRole::Incoming => 0.0,
        };
    }
    if progress == 1.0 {
        return match transition.role {
            AudioTransitionRole::Outgoing => 0.0,
            AudioTransitionRole::Incoming => 1.0,
        };
    }
    let angle = std::f64::consts::FRAC_PI_2 * progress;
    match transition.role {
        AudioTransitionRole::Outgoing => angle.cos() as f32,
        AudioTransitionRole::Incoming => angle.sin() as f32,
    }
}

fn shaped_fade(outer_to_full: f32, curve: f32) -> f32 {
    let t = outer_to_full.clamp(0.0, 1.0);
    let control = 0.5 + curve.clamp(-1.0, 1.0) * 0.5;
    let one_minus = 1.0 - t;
    (2.0 * one_minus * t * control + t * t).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    fn generated_ramp_wav(sample_rate: u32, seconds: u32) -> PathBuf {
        let frames = sample_rate.saturating_mul(seconds);
        let data_bytes = frames.saturating_mul(2);
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36_u32.saturating_add(data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&sample_rate.saturating_mul(2).to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..frames {
            let progress = frame as f32 / frames.max(1) as f32;
            let sample = ((progress * 1.6 - 0.8) * i16::MAX as f32) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "maelstrom-sticky-audio-{}-{nonce}.wav",
            std::process::id()
        ));
        fs::write(&path, wav).expect("write generated ramp wav");
        path
    }

    fn ramp_value_at_tick(tick: i64, duration_ticks: i64) -> f32 {
        ((tick.max(0) as f32 / duration_ticks.max(1) as f32) * 1.6 - 0.8).clamp(-0.8, 0.8)
    }

    fn test_lane(samples: impl Into<VecDeque<f32>>) -> HashMap<LaneKey, Lane> {
        HashMap::from([(
            LaneKey {
                track_id: 1,
                clip_id: 1,
            },
            Lane {
                samples: samples.into(),
                ..Default::default()
            },
        )])
    }

    #[test]
    fn stereo_width_processes_mid_side_without_reordering_channels() {
        let mut processors =
            build_processors(&[AudioProcessorSpec::StereoWidth { width: 0.0 }], 48_000);
        let mut left = 1.0;
        let mut right = -0.5;
        processors[0].process(&mut left, &mut right);
        assert!((left - 0.25).abs() < 0.000_01);
        assert!((right - 0.25).abs() < 0.000_01);
    }

    #[test]
    fn ordered_filters_run_before_gain_and_produce_finite_output() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: -6.020_6,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: vec![
                AudioProcessorSpec::HighPass { hz: 200 },
                AudioProcessorSpec::LowPass { hz: 4_000 },
                AudioProcessorSpec::Eq { hz: 1_000, db: 6.0 },
            ],
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 1_000_000,
            transition: None,
        };
        let (left_gain, right_gain) = channel_gains(&target);
        let mut shared = Shared {
            lanes: HashMap::from([(
                LaneKey::from(&target),
                Lane {
                    samples: VecDeque::from([1.0, -1.0]),
                    target: Some(target.clone()),
                    gain_left_linear: left_gain,
                    gain_right_linear: right_gain,
                    processors: build_processors(&target.effects, 48_000),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };
        let (left, right, audible) = shared.pop_stereo_frame(true, 48_000);
        assert!(audible && left.is_finite() && right.is_finite());
        // Gain is applied after the processor chain, so a finite filtered impulse remains below
        // the unity input when the lane has a -6 dB trim.
        assert!(left.abs() < 0.5 && right.abs() < 0.5);
    }

    #[test]
    fn live_effect_changes_rebuild_processor_state_without_resetting_pcm() {
        let mut target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: vec![AudioProcessorSpec::LowPass { hz: 1_000 }],
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 1_000_000,
            transition: None,
        };
        let (left_gain, right_gain) = channel_gains(&target);
        let key = LaneKey::from(&target);
        let mut shared = Shared {
            lanes: HashMap::from([(
                key,
                Lane {
                    samples: VecDeque::from([1.0, 1.0, 0.0, 0.0]),
                    target: Some(target.clone()),
                    gain_left_linear: left_gain,
                    gain_right_linear: right_gain,
                    processors: build_processors(&target.effects, 48_000),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };
        let _ = shared.pop_stereo_frame(true, 48_000);
        target.effects = vec![AudioProcessorSpec::StereoWidth { width: 0.0 }];
        assert!(shared.update_mix_settings(&[target], 48_000));
        let lane = shared.lanes.get(&key).expect("retained lane");
        assert_eq!(lane.samples.len(), 2);
        assert!(matches!(
            lane.processors.as_slice(),
            [LaneProcessor::StereoWidth { .. }]
        ));
        let (left, right, audible) = shared.pop_stereo_frame(true, 48_000);
        assert!(audible);
        assert!(left.abs() < 0.000_01 && right.abs() < 0.000_01);
    }

    #[test]
    fn fade_envelope_reaches_full_level_and_silence_at_edges() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 1_000_000,
            fade_in_curve: 0.0,
            fade_out_ticks: 1_000_000,
            fade_out_curve: 0.0,
            clip_duration_ticks: 4_000_000,
            transition: None,
        };
        assert_eq!(fade_envelope(0, &target), 0.0);
        assert_eq!(fade_envelope(1_000_000, &target), 1.0);
        assert_eq!(fade_envelope(3_000_000, &target), 1.0);
        assert_eq!(fade_envelope(4_000_000, &target), 0.0);
    }

    #[test]
    fn audible_fade_uses_the_same_adjustable_curve_as_the_timeline_envelope() {
        assert!((shaped_fade(0.5, -1.0) - 0.25).abs() < f32::EPSILON);
        assert!((shaped_fade(0.5, 0.0) - 0.5).abs() < f32::EPSILON);
        assert!((shaped_fade(0.5, 1.0) - 0.75).abs() < f32::EPSILON);
        assert_eq!(shaped_fade(0.0, 1.0), 0.0);
        assert_eq!(shaped_fade(1.0, -1.0), 1.0);
    }

    #[test]
    fn equal_power_transition_has_exact_endpoints_and_constant_energy() {
        let outgoing = AudioTransitionEnvelope {
            role: AudioTransitionRole::Outgoing,
            start_clip_tick: 500_000,
            duration_ticks: 1_000_000,
        };
        let incoming = AudioTransitionEnvelope {
            role: AudioTransitionRole::Incoming,
            ..outgoing
        };
        assert_eq!(transition_envelope(500_000, Some(outgoing)), 1.0);
        assert_eq!(transition_envelope(1_500_000, Some(outgoing)), 0.0);
        assert_eq!(transition_envelope(500_000, Some(incoming)), 0.0);
        assert_eq!(transition_envelope(1_500_000, Some(incoming)), 1.0);

        for progress_tick in [0, 125_000, 500_000, 875_000, 1_000_000] {
            let tick = 500_000 + progress_tick;
            let energy = transition_envelope(tick, Some(outgoing)).powi(2)
                + transition_envelope(tick, Some(incoming)).powi(2);
            assert!(
                (energy - 1.0).abs() < 0.000_001,
                "energy={energy} at tick {tick}"
            );
        }
        assert_eq!(
            transition_envelope(
                500_000,
                Some(AudioTransitionEnvelope {
                    duration_ticks: 0,
                    ..outgoing
                })
            ),
            1.0
        );
    }

    #[test]
    fn transition_gain_multiplies_ordinary_clip_fades_per_device_sample() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 1_000_000,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 2_000_000,
            transition: Some(AudioTransitionEnvelope {
                role: AudioTransitionRole::Outgoing,
                start_clip_tick: 0,
                duration_ticks: 1_000_000,
            }),
        };
        let mut state = Shared {
            lanes: HashMap::from([(
                LaneKey::from(&target),
                Lane {
                    samples: VecDeque::from([1.0, 1.0]),
                    target: Some(target),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };
        state.device_frames.store(24_000, Ordering::Release);
        let (left, right, audible) = state.pop_stereo_frame(true, 48_000);
        let expected = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((left - expected).abs() < 0.000_001);
        assert!((right - expected).abs() < 0.000_001);
        assert!(audible);
    }

    #[test]
    fn transition_envelope_advances_with_each_device_frame() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 2_000_000,
            transition: Some(AudioTransitionEnvelope {
                role: AudioTransitionRole::Incoming,
                start_clip_tick: 0,
                duration_ticks: 1_000_000,
            }),
        };
        let mut state = Shared {
            lanes: HashMap::from([(
                LaneKey::from(&target),
                Lane {
                    samples: VecDeque::from([1.0, 1.0, 1.0, 1.0]),
                    target: Some(target),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };
        assert_eq!(state.pop_stereo_frame(true, 1), (0.0, 0.0, true));
        advance_device_clock(&state.device_frames, true, 1);
        assert_eq!(state.pop_stereo_frame(true, 1), (1.0, 1.0, true));
    }

    #[test]
    fn reconcile_lanes_preserves_transport_and_starts_new_lane_at_current_clock() {
        let retained = AudioTarget {
            track_id: 1,
            clip_id: 10,
            path: PathBuf::from("retained.wav"),
            source_tick: 2_000_000,
            clip_tick: 2_000_000,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: None,
        };
        let joining = AudioTarget {
            track_id: 1,
            clip_id: 11,
            path: PathBuf::from("joining.wav"),
            source_tick: 7_000_000,
            clip_tick: 5_000_000,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: Some(AudioTransitionEnvelope {
                role: AudioTransitionRole::Incoming,
                start_clip_tick: 5_000_000,
                duration_ticks: 1_000_000,
            }),
        };
        let retained_key = LaneKey::from(&retained);
        let joining_key = LaneKey::from(&joining);
        let mut state = Shared {
            lanes: HashMap::from([(
                retained_key,
                Lane {
                    samples: VecDeque::from([0.25, -0.25]),
                    decoded_frames: 9,
                    target: Some(retained.clone()),
                    ..Lane::default()
                },
            )]),
            generation: 44,
            ..Shared::default()
        };
        state.device_frames.store(48_000, Ordering::Release);

        let (generation, resumed) = state
            .reconcile_targets(&[retained.clone(), joining.clone()], 48_000)
            .expect("same-media lane expansion is safe");
        assert_eq!(resumed, HashSet::from([retained_key]));
        assert_eq!(generation, 45);
        assert_eq!(state.generation, 45);
        assert_eq!(state.device_frames.load(Ordering::Acquire), 48_000);
        let retained_lane = state.lanes.get(&retained_key).unwrap();
        assert_eq!(retained_lane.samples, VecDeque::from([0.25, -0.25]));
        assert_eq!(retained_lane.decoded_frames, 9);

        let joining_lane = state.lanes.get(&joining_key).unwrap();
        assert_eq!(joining_lane.device_frame_origin, 48_000);
        assert_eq!(joining_lane.target.as_ref().unwrap().source_tick, 7_000_000);
        enqueue_decoded_frame(&mut state, joining_key, &[1.0, 1.0, 1.0, 1.0], 48_000).unwrap();
        assert_eq!(state.lanes.get(&joining_key).unwrap().samples.len(), 4);
        state.lanes.get_mut(&retained_key).unwrap().samples.clear();
        assert_eq!(state.pop_stereo_frame(true, 48_000), (0.0, 0.0, true));
        advance_device_clock(&state.device_frames, true, 24_000);
        let (left, right, audible) = state.pop_stereo_frame(true, 48_000);
        assert!((left - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.000_001);
        assert!((right - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.000_001);
        assert!(audible);

        let (generation, resumed) = state
            .reconcile_targets(&[retained], 48_000)
            .expect("same-media lane contraction is safe");
        assert_eq!(resumed, HashSet::from([retained_key]));
        assert_eq!(state.lanes.len(), 1);
        assert_eq!(generation, 46);
        assert_eq!(state.generation, 46);
        assert_eq!(state.device_frames.load(Ordering::Acquire), 72_000);
    }

    #[test]
    fn same_track_overlap_uses_independent_lane_keys_and_mixes() {
        let make_target = |clip_id| AudioTarget {
            track_id: 7,
            clip_id,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: -6.020_6,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 1_000_000,
            transition: None,
        };
        let first = make_target(100);
        let second = make_target(101);
        let mut sessions: HashMap<LaneKey, ()> =
            HashMap::from([(LaneKey::from(&first), ()), (LaneKey::from(&second), ())]);
        assert_eq!(sessions.len(), 2);
        sessions.retain(|key, _| key.track_id == 7);

        let mut state = Shared {
            lanes: HashMap::from([
                (
                    LaneKey::from(&first),
                    Lane {
                        samples: VecDeque::from([1.0, 1.0]),
                        target: Some(first),
                        ..Lane::default()
                    },
                ),
                (
                    LaneKey::from(&second),
                    Lane {
                        samples: VecDeque::from([1.0, 1.0]),
                        target: Some(second),
                        ..Lane::default()
                    },
                ),
            ]),
            ..Shared::default()
        };
        assert_eq!(state.pop_stereo_frame(true, 48_000), (1.0, 1.0, true));
    }

    #[test]
    fn live_mix_gain_updates_queued_pcm_without_a_decode_restart() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from("clip.mp4"),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: None,
        };
        let key = LaneKey::from(&target);
        let mut state = Shared {
            lanes: HashMap::from([(
                key,
                Lane {
                    samples: VecDeque::from([1.0, -1.0, 1.0, -1.0]),
                    target: Some(target.clone()),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };

        assert_eq!(state.pop_stereo_frame(true, 48_000), (1.0, -1.0, true));
        let mut quieter = target;
        quieter.gain_db = -6.020_6;
        assert!(state.update_mix_settings(&[quieter], 48_000));
        let mixed = state.pop_stereo_frame(true, 48_000);
        assert!((mixed.0 - 0.5).abs() < 0.000_1);
        assert!((mixed.1 + 0.5).abs() < 0.000_1);
    }

    #[test]
    fn live_mix_pan_and_channel_trim_update_queued_pcm() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from("clip.mp4"),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: None,
        };
        let key = LaneKey::from(&target);
        let mut state = Shared {
            lanes: HashMap::from([(
                key,
                Lane {
                    samples: VecDeque::from([1.0, 1.0, 1.0, 1.0]),
                    target: Some(target.clone()),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };

        let mut panned = target;
        panned.pan = 1.0;
        panned.gain_left_db = -6.020_6;
        assert!(state.update_mix_settings(&[panned], 48_000));
        let mixed = state.pop_stereo_frame(true, 48_000);
        assert!(mixed.0.abs() < 0.000_1);
        assert!((mixed.1 - 1.0).abs() < 0.000_1);
    }

    #[test]
    fn live_mix_fade_updates_queued_pcm_without_a_decode_restart() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from("clip.mp4"),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: None,
        };
        let key = LaneKey::from(&target);
        let mut state = Shared {
            lanes: HashMap::from([(
                key,
                Lane {
                    samples: VecDeque::from([1.0, -1.0, 1.0, -1.0]),
                    target: Some(target.clone()),
                    ..Lane::default()
                },
            )]),
            ..Shared::default()
        };
        state.device_frames.store(24_000, Ordering::Release);

        assert_eq!(state.pop_stereo_frame(true, 48_000), (1.0, -1.0, true));
        let mut fading = target;
        fading.fade_in_ticks = 1_000_000;
        assert!(state.update_mix_settings(&[fading], 48_000));
        let mixed = state.pop_stereo_frame(true, 48_000);
        assert!((mixed.0 - 0.5).abs() < 0.000_1);
        assert!((mixed.1 + 0.5).abs() < 0.000_1);
    }

    #[test]
    fn pending_audio_seek_is_a_single_latest_target_slot() {
        let target = |source_tick| AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from("clip.mp4"),
            source_tick,
            clip_tick: source_tick,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 15_000_000,
            transition: None,
        };
        let mut scheduler = SchedulerState::default();
        for generation in 1..=100 {
            scheduler.submit(generation, vec![target(generation as i64 * 10_000)]);
        }
        let pending = scheduler.pending.expect("latest target remains pending");
        assert_eq!(pending.generation, 100);
        assert_eq!(pending.targets.len(), 1);
        assert_eq!(pending.targets[0].source_tick, 1_000_000);
        assert!(pending.resume_lanes.is_empty());
    }

    #[test]
    fn reconcile_decode_job_marks_only_retained_lanes_for_resume() {
        let target = |clip_id| AudioTarget {
            track_id: 1,
            clip_id,
            path: PathBuf::from("clip.mp4"),
            source_tick: 1_000_000,
            clip_tick: 1_000_000,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 15_000_000,
            transition: None,
        };
        let retained = target(10);
        let joining = target(11);
        let mut scheduler = SchedulerState::default();
        scheduler.submit_reconcile(
            8,
            vec![retained.clone(), joining],
            HashSet::from([LaneKey::from(&retained)]),
        );
        let pending = scheduler.pending.expect("reconcile job queued");
        assert_eq!(pending.generation, 8);
        assert_eq!(
            pending.resume_lanes,
            HashSet::from([LaneKey::from(&retained)])
        );
    }

    #[test]
    fn sticky_audio_policy_continues_only_nearby_forward_targets() {
        let target = |path: &str, source_tick| AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from(path),
            source_tick,
            clip_tick: source_tick,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 15_000_000,
            transition: None,
        };
        let first = target("clip.mp4", 0);
        assert_eq!(
            session_action(None, None, None, &first, 48_000),
            SessionAction::Open
        );
        assert_eq!(
            session_action(
                Some(Path::new("clip.mp4")),
                Some(48_000),
                Some(0),
                &target("clip.mp4", 250_000),
                48_000
            ),
            SessionAction::Continue
        );
        assert_eq!(
            session_action(
                Some(Path::new("clip.mp4")),
                Some(48_000),
                Some(250_000),
                &first,
                48_000
            ),
            SessionAction::SeekAndFlush
        );
        assert_eq!(
            session_action(
                Some(Path::new("clip.mp4")),
                Some(48_000),
                Some(0),
                &target("clip.mp4", FORWARD_REUSE_TICKS + 1),
                48_000
            ),
            SessionAction::SeekAndFlush
        );
        assert_eq!(
            session_action(
                Some(Path::new("clip.mp4")),
                Some(48_000),
                Some(0),
                &target("other.mp4", 250_000),
                48_000
            ),
            SessionAction::Open
        );
        assert_eq!(
            session_action(
                Some(Path::new("clip.mp4")),
                Some(48_000),
                Some(0),
                &target("clip.mp4", 250_000),
                44_100
            ),
            SessionAction::Open
        );
    }

    #[test]
    fn live_meter_sanitizes_non_finite_and_out_of_range_samples() {
        let meter = AudioMeter::default();
        meter.store(f32::NAN, -4.0);
        assert_eq!(meter.load(), (0.0, 1.0));
        meter.clear();
        assert_eq!(meter.load(), (0.0, 0.0));
    }

    #[test]
    fn runtime_diagnostics_batch_playing_underruns_and_ignore_paused_callbacks() {
        let diagnostics = AudioRuntimeCounters::default();
        diagnostics.record_callback_lock_failure();
        diagnostics.record_underrun_frames(false, 480);
        diagnostics.record_underrun_frames(true, 480);
        diagnostics.record_underrun_frames(true, 0);
        diagnostics.record_late_discard(12);

        assert_eq!(
            diagnostics.snapshot(),
            AudioRuntimeDiagnostics {
                output_callback_cpu_timing: AudioCallbackCpuTiming::default(),
                callback_lock_failures: 1,
                underrun_device_frames: 480,
                late_decoded_frames_discarded: 12,
            }
        );
    }

    #[test]
    fn output_callback_cpu_timing_is_empty_by_default() {
        assert_eq!(
            AudioRuntimeCounters::default()
                .snapshot()
                .output_callback_cpu_timing,
            AudioCallbackCpuTiming::default()
        );
    }

    #[test]
    fn output_callback_cpu_timing_records_total_and_max_with_saturation() {
        let diagnostics = AudioRuntimeCounters::default();
        diagnostics.record_output_callback_cpu_nanos(7);
        diagnostics.record_output_callback_cpu_nanos(11);

        assert_eq!(
            diagnostics.snapshot().output_callback_cpu_timing,
            AudioCallbackCpuTiming {
                samples: 2,
                total_nanos: 18,
                max_nanos: 11,
            }
        );

        diagnostics
            .output_callback_cpu_total_nanos
            .store(u64::MAX - 2, Ordering::Relaxed);
        diagnostics
            .output_callback_cpu_samples
            .store(u64::MAX, Ordering::Relaxed);
        diagnostics.record_output_callback_cpu_nanos(3);

        assert_eq!(
            diagnostics.snapshot().output_callback_cpu_timing,
            AudioCallbackCpuTiming {
                samples: u64::MAX,
                total_nanos: u64::MAX,
                max_nanos: 11,
            }
        );
    }

    #[test]
    fn output_callback_cpu_timing_snapshots_are_coherent_during_writes() {
        const RECORDS: u64 = 100_000;
        const NANOS_PER_RECORD: u64 = 3;

        let diagnostics = Arc::new(AudioRuntimeCounters::default());
        let writer_diagnostics = Arc::clone(&diagnostics);
        let writer = thread::spawn(move || {
            for _ in 0..RECORDS {
                writer_diagnostics.record_output_callback_cpu_nanos(NANOS_PER_RECORD);
            }
        });

        while !writer.is_finished() {
            let timing = diagnostics.snapshot().output_callback_cpu_timing;
            assert_eq!(timing.total_nanos, timing.samples * NANOS_PER_RECORD);
            assert!(timing.max_nanos <= NANOS_PER_RECORD);
        }
        writer.join().expect("timing writer should finish");

        assert_eq!(
            diagnostics.snapshot().output_callback_cpu_timing,
            AudioCallbackCpuTiming {
                samples: RECORDS,
                total_nanos: RECORDS * NANOS_PER_RECORD,
                max_nanos: NANOS_PER_RECORD,
            }
        );
    }

    #[test]
    fn partial_lane_underrun_counts_only_silent_device_frames() {
        let mut state = Shared {
            lanes: test_lane(VecDeque::from([0.25, -0.25])),
            ..Default::default()
        };
        let diagnostics = Arc::clone(&state.diagnostics);
        let mut silent_frames = 0;
        for _ in 0..3 {
            let (_, _, audible) = state.pop_stereo_frame(true, 48_000);
            silent_frames += usize::from(!audible);
        }
        diagnostics.record_underrun_frames(true, silent_frames);

        assert_eq!(
            diagnostics.snapshot().underrun_device_frames,
            2,
            "one ready frame must not turn a partially ready callback into a full underrun"
        );
    }

    #[test]
    fn device_clock_tracks_native_callback_and_marks_underrun_silence() {
        let mut state = Shared {
            lanes: test_lane(VecDeque::from([0.25, -0.25, 0.5, -0.5])),
            ..Default::default()
        };
        assert_eq!(playback_source_tick(2_000_000, 0, 48_000), None);
        assert_eq!(state.pop_stereo_frame(false, 48_000), (0.0, 0.0, false));
        assert_eq!(state.device_frames.load(Ordering::Acquire), 0);
        assert_eq!(state.pop_stereo_frame(true, 48_000), (0.25, -0.25, true));
        advance_device_clock(&state.device_frames, true, 1);
        assert_eq!(playback_source_tick(2_000_000, 1, 48_000), Some(2_000_020));
        assert_eq!(state.pop_stereo_frame(true, 48_000), (0.5, -0.5, true));
        advance_device_clock(&state.device_frames, true, 1);
        assert_eq!(playback_source_tick(2_000_000, 2, 48_000), Some(2_000_041));
        assert_eq!(state.pop_stereo_frame(true, 48_000), (0.0, 0.0, false));
        advance_device_clock(&state.device_frames, true, 1);
        assert_eq!(
            state.device_frames.load(Ordering::Acquire),
            3,
            "device silence still advances the shared A/V clock"
        );
        assert_eq!(playback_source_tick(2_000_000, 3, 48_000), Some(2_000_062));
        assert_eq!(stale_frames_to_skip(4_800, 0), 4_800);
        assert_eq!(stale_frames_to_skip(4_800, 4_800), 0);
        assert_eq!(stale_frames_to_skip(4_800, 5_000), 0);
    }

    #[test]
    fn device_clock_advances_even_when_the_pcm_queue_lock_is_contended() {
        let shared = Mutex::new(Shared::default());
        let _held_by_decoder = shared.lock().unwrap();
        assert!(try_lock_callback(&shared).is_none());
        let clock = AtomicU64::new(0);
        advance_device_clock(&clock, true, 480);
        assert_eq!(clock.load(Ordering::Acquire), 480);
        assert_eq!(
            playback_source_tick(1_000_000, 480, 48_000),
            Some(1_010_000)
        );
    }

    #[test]
    fn callback_lock_retry_acquires_an_uncontended_mutex_immediately() {
        let mutex = Mutex::new(7_u8);

        let guard = try_lock_callback(&mutex).expect("uncontended callback lock");

        assert_eq!(*guard, 7);
    }

    #[test]
    fn callback_lock_retry_exhausts_while_mutex_is_held() {
        let mutex = Mutex::new(());
        let _held = mutex.lock().expect("hold mutex for retry exhaustion");

        assert!(try_lock_callback(&mutex).is_none());
    }

    #[test]
    fn active_lanes_mix_and_clamp_on_one_device_clock() {
        let mut state = Shared {
            lanes: HashMap::from([
                (
                    LaneKey {
                        track_id: 1,
                        clip_id: 1,
                    },
                    Lane {
                        samples: VecDeque::from([0.75, -0.5]),
                        ..Default::default()
                    },
                ),
                (
                    LaneKey {
                        track_id: 2,
                        clip_id: 2,
                    },
                    Lane {
                        samples: VecDeque::from([0.75, -0.75]),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        assert_eq!(state.pop_stereo_frame(true, 48_000), (1.0, -1.0, true));
        advance_device_clock(&state.device_frames, true, 1);
        assert_eq!(state.device_frames.load(Ordering::Acquire), 1);
    }

    #[test]
    fn missing_lane_is_silence_without_stalling_ready_lane() {
        let mut state = Shared {
            lanes: HashMap::from([
                (
                    LaneKey {
                        track_id: 1,
                        clip_id: 1,
                    },
                    Lane {
                        samples: VecDeque::from([0.25, -0.25]),
                        ..Default::default()
                    },
                ),
                (
                    LaneKey {
                        track_id: 2,
                        clip_id: 2,
                    },
                    Lane::default(),
                ),
            ]),
            ..Default::default()
        };
        assert_eq!(state.pop_stereo_frame(true, 48_000), (0.25, -0.25, true));
        assert_eq!(state.pop_stereo_frame(true, 48_000), (0.0, 0.0, false));
        advance_device_clock(&state.device_frames, true, 2);
        assert_eq!(state.device_frames.load(Ordering::Acquire), 2);
    }

    #[test]
    fn late_lane_discards_only_elapsed_frames_and_keeps_its_decode_cursor_aligned() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: None,
        };
        let key = LaneKey::from(&target);
        let mut state = Shared {
            lanes: test_lane(VecDeque::new()),
            ..Default::default()
        };
        state.device_frames.store(2, Ordering::Release);

        enqueue_decoded_frame(
            &mut state,
            key,
            &[0.1, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4],
            48_000,
        )
        .unwrap();

        let lane = state.lanes.get(&key).unwrap();
        assert_eq!(lane.samples, VecDeque::from([0.3, -0.3, 0.4, -0.4]));
        assert_eq!(lane.decoded_frames, 4);
        assert_eq!(
            state.diagnostics.snapshot().late_decoded_frames_discarded,
            2
        );
    }

    #[test]
    fn total_pcm_budget_is_split_between_active_lanes() {
        assert_eq!(lane_capacity_samples(48_000, 1), 96_000);
        assert_eq!(lane_capacity_samples(48_000, 4), 24_000);
        assert_eq!(lane_capacity_samples(48_000, 7) % 2, 0);
        assert!(lane_capacity_samples(1, 100) >= 2);
    }

    #[test]
    fn decoded_lane_cannot_grow_beyond_mix_budget_plus_one_bounded_frame() {
        let target = AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::new(),
            source_tick: 0,
            clip_tick: 0,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 10_000_000,
            transition: None,
        };
        let key = LaneKey::from(&target);
        let mut state = Shared {
            lanes: test_lane(VecDeque::from([0.0, 0.0])),
            ..Default::default()
        };
        let oversized = vec![0.0; MAX_DECODED_AUDIO_FRAME_SAMPLES + 2];
        assert!(enqueue_decoded_frame(&mut state, key, &oversized, 1).is_err());
        assert_eq!(state.lanes[&key].samples.len(), 2);
    }

    #[test]
    fn supplied_media_produces_non_silent_pcm() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let shared = Arc::new(Mutex::new(Shared {
            lanes: test_lane(VecDeque::new()),
            generation: 1,
            ..Default::default()
        }));
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            decode_into_queue(
                worker_shared,
                1,
                AudioTarget {
                    track_id: 1,
                    clip_id: 1,
                    path: PathBuf::from(path),
                    source_tick: 2_000_000,
                    clip_tick: 2_000_000,
                    gain_db: 0.0,
                    gain_left_db: 0.0,
                    gain_right_db: 0.0,
                    pan: 0.0,
                    effects: Vec::new(),
                    fade_in_ticks: 0,
                    fade_in_curve: 0.0,
                    fade_out_ticks: 0,
                    fade_out_curve: 0.0,
                    clip_duration_ticks: 15_000_000,
                    transition: None,
                },
                48_000,
            )
        });
        let started = Instant::now();
        let mut has_audio = false;
        while started.elapsed() < Duration::from_secs(3) {
            {
                let state = shared.lock().unwrap();
                has_audio = state
                    .lanes
                    .values()
                    .flat_map(|lane| &lane.samples)
                    .any(|sample| sample.abs() > 0.000_01);
            }
            if has_audio {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        {
            let mut state = shared.lock().unwrap();
            state.generation = 2;
            state.lanes.clear();
        }
        worker.join().unwrap().unwrap();
        assert!(has_audio, "supplied media produced no audible PCM samples");
    }

    #[test]
    fn supplied_media_two_lanes_decode_and_mix_at_the_same_device_frame() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let make_target = |track_id, clip_id| AudioTarget {
            track_id,
            clip_id,
            path: PathBuf::from(&path),
            source_tick: 2_000_000,
            clip_tick: 2_000_000,
            gain_db: -6.020_6,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 15_000_000,
            transition: None,
        };
        let first = make_target(1, 1);
        let second = make_target(2, 2);
        let shared = Arc::new(Mutex::new(Shared {
            lanes: HashMap::from([
                (LaneKey::from(&first), Lane::default()),
                (LaneKey::from(&second), Lane::default()),
            ]),
            generation: 1,
            ..Default::default()
        }));

        decode_into_queue(Arc::clone(&shared), 1, first, 48_000).unwrap();
        decode_into_queue(Arc::clone(&shared), 1, second, 48_000).unwrap();

        let mut state = shared.lock().unwrap();
        let fronts: Vec<_> = state
            .lanes
            .values()
            .map(|lane| {
                assert!(lane.samples.len() >= 2);
                (lane.samples[0], lane.samples[1])
            })
            .collect();
        let expected = (
            (fronts[0].0 + fronts[1].0).clamp(-1.0, 1.0),
            (fronts[0].1 + fronts[1].1).clamp(-1.0, 1.0),
        );
        let mixed = state.pop_stereo_frame(true, 48_000);
        assert_eq!((mixed.0, mixed.1), expected);
        assert!(mixed.2);
    }

    #[test]
    fn supplied_media_sticky_session_reuses_forward_and_seeks_backward() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let target = |source_tick| AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from(&path),
            source_tick,
            clip_tick: source_tick,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 15_000_000,
            transition: None,
        };
        fn decode_briefly(
            session: &mut StickyAudio,
            shared: Arc<Mutex<Shared>>,
            target: &AudioTarget,
        ) -> SessionAction {
            {
                let mut state = shared.lock().expect("audio state lock");
                let lane = state.lanes.entry(LaneKey::from(target)).or_default();
                lane.samples.clear();
                lane.decoded_frames = 0;
                state.device_frames.store(0, Ordering::Release);
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&cancelled);
            let stopper = thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                stop.store(true, Ordering::Release);
            });
            let action = session
                .decode_into_queue(shared, target, LaneKey::from(target), false, |_| {
                    cancelled.load(Ordering::Acquire)
                })
                .expect("decode supplied media");
            stopper.join().expect("decode stopper");
            action
        }

        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut session = StickyAudio::open(&target(0), 48_000).expect("open audio session");
        assert_eq!(
            decode_briefly(&mut session, Arc::clone(&shared), &target(0)),
            SessionAction::SeekAndFlush
        );
        assert_eq!(session.seek_count, 1);
        assert!(
            shared
                .lock()
                .expect("audio state lock")
                .samples
                .iter()
                .any(|sample| sample.abs() > 0.000_01),
            "initial sticky decode produced no PCM"
        );

        let forward_tick = session
            .decoder_cursor_tick
            .expect("decode records its post-output source position")
            .saturating_add(100_000);
        shared.lock().expect("audio state lock").samples.clear();
        assert_eq!(
            decode_briefly(&mut session, Arc::clone(&shared), &target(forward_tick)),
            SessionAction::Continue
        );
        assert_eq!(
            session.seek_count, 1,
            "near forward target reused the open session"
        );
        assert!(
            shared
                .lock()
                .expect("audio state lock")
                .samples
                .iter()
                .any(|sample| sample.abs() > 0.000_01),
            "continued sticky decode produced no PCM"
        );

        shared.lock().expect("audio state lock").samples.clear();
        assert_eq!(
            decode_briefly(&mut session, Arc::clone(&shared), &target(0)),
            SessionAction::SeekAndFlush
        );
        assert_eq!(
            session.seek_count, 2,
            "backward target flushed the sticky session"
        );
        assert!(
            shared
                .lock()
                .expect("audio state lock")
                .samples
                .iter()
                .any(|sample| sample.abs() > 0.000_01),
            "backward sticky decode produced no PCM"
        );
    }

    #[test]
    fn sticky_session_aligns_rate_converted_pcm_after_reuse_and_backward_seek() {
        const INPUT_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const DURATION_TICKS: i64 = 4_000_000;
        let path = generated_ramp_wav(INPUT_RATE, 4);
        let target = |source_tick| AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: path.clone(),
            source_tick,
            clip_tick: source_tick,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: DURATION_TICKS,
            transition: None,
        };
        let decode_window =
            |session: &mut StickyAudio, shared: &Arc<Mutex<Shared>>, target: &AudioTarget| {
                let mut state = shared.lock().expect("audio state lock");
                state.samples.clear();
                state
                    .lanes
                    .entry(LaneKey::from(target))
                    .or_default()
                    .samples
                    .clear();
                drop(state);
                let action = session
                    .decode_into_queue(
                        Arc::clone(shared),
                        target,
                        LaneKey::from(target),
                        false,
                        |state| state.samples.len() >= 4_096,
                    )
                    .expect("decode rate-converted window");
                let first = shared
                    .lock()
                    .expect("audio state lock")
                    .samples
                    .front()
                    .copied()
                    .expect("decoded window contains PCM");
                (action, first)
            };

        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut session = StickyAudio::open(&target(0), OUTPUT_RATE).expect("open 44.1 kHz wav");
        let (initial_action, _) = decode_window(&mut session, &shared, &target(0));
        assert_eq!(initial_action, SessionAction::SeekAndFlush);

        let forward_tick = session
            .decoder_cursor_tick
            .expect("initial decode advances the decoder cursor")
            .saturating_add(250_000);
        let (forward_action, forward_first) =
            decode_window(&mut session, &shared, &target(forward_tick));
        let backward_tick = 100_000;
        let (backward_action, backward_first) =
            decode_window(&mut session, &shared, &target(backward_tick));
        let _ = fs::remove_file(&path);

        assert_eq!(forward_action, SessionAction::Continue);
        let forward_expected =
            ramp_value_at_tick(forward_tick, DURATION_TICKS) * std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (forward_first - forward_expected).abs() < 0.04,
            "continued PCM started at the wrong source position: target={forward_tick} sample={forward_first}"
        );
        assert_eq!(backward_action, SessionAction::SeekAndFlush);
        let backward_expected =
            ramp_value_at_tick(backward_tick, DURATION_TICKS) * std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (backward_first - backward_expected).abs() < 0.04,
            "backward PCM started at the wrong source position: sample={backward_first}"
        );
    }

    #[test]
    fn supplied_media_latest_of_one_hundred_seeks_reaches_output() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let engine = AudioEngine::new().expect("default audio output device");
        for index in 0..100_i64 {
            let tick = index * 100_000;
            engine.seek_and_play(AudioTarget {
                track_id: 1,
                clip_id: 1,
                path: PathBuf::from(&path),
                source_tick: tick,
                clip_tick: tick,
                gain_db: 0.0,
                gain_left_db: 0.0,
                gain_right_db: 0.0,
                pan: 0.0,
                effects: Vec::new(),
                fade_in_ticks: 0,
                fade_in_curve: 0.0,
                fade_out_ticks: 0,
                fade_out_curve: 0.0,
                clip_duration_ticks: 15_000_000,
                transition: None,
            });
        }
        let started = Instant::now();
        let mut reached_output = false;
        while started.elapsed() < Duration::from_secs(3) {
            let (left, right) = engine.meter_levels();
            if left > 0.000_01 || right > 0.000_01 {
                reached_output = true;
                break;
            }
            if let Some(error) = engine.take_error() {
                panic!("latest audio seek failed: {error}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        engine.pause();
        assert!(reached_output, "latest coalesced seek never reached output");
    }

    #[test]
    fn supplied_media_device_clock_stays_within_one_24fps_frame() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let engine = AudioEngine::new().expect("default audio output device");
        engine.seek_and_play(AudioTarget {
            track_id: 1,
            clip_id: 1,
            path: PathBuf::from(path),
            source_tick: 1_000_000,
            clip_tick: 1_000_000,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            fade_in_ticks: 0,
            fade_in_curve: 0.0,
            fade_out_ticks: 0,
            fade_out_curve: 0.0,
            clip_duration_ticks: 15_000_000,
            transition: None,
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        let (first_tick, first_wall) = loop {
            if let Some(tick) = engine.playback_source_tick() {
                break (tick, Instant::now());
            }
            if let Some(error) = engine.take_error() {
                panic!("audio clock probe failed: {error}");
            }
            assert!(
                Instant::now() < deadline,
                "device consumed no audio before timeout"
            );
            thread::sleep(Duration::from_millis(2));
        };

        let (last_tick, last_wall) = loop {
            let now = Instant::now();
            if let Some(tick) = engine.playback_source_tick()
                && tick.saturating_sub(first_tick) >= 500_000
            {
                break (tick, now);
            }
            assert!(now < deadline, "device clock did not advance 500 ms");
            thread::sleep(Duration::from_millis(2));
        };
        engine.pause();
        let device_elapsed = last_tick.saturating_sub(first_tick);
        let wall_elapsed = last_wall
            .duration_since(first_wall)
            .as_micros()
            .min(i64::MAX as u128) as i64;
        assert!(
            device_elapsed.saturating_sub(wall_elapsed).abs() <= 41_667,
            "device clock drifted more than one 24 fps frame: device={device_elapsed}us wall={wall_elapsed}us"
        );
    }

    #[test]
    fn default_output_device_opens() {
        let Ok(engine) = AudioEngine::new() else {
            panic!("default audio output device could not be opened");
        };
        engine.pause();
    }
}
