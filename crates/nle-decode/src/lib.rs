//! Latest-wins in-process FFmpeg monitor decoding.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering, fence},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ffmpeg::{
    codec::Id,
    format::Pixel,
    media::Type,
    software::scaling::{context::Context as ScalingContext, flag::Flags as ScalingFlags},
    util::{frame::video::Video, mathematics::Rescale},
};
use ffmpeg_next as ffmpeg;
use nle_cache::{FrameCache, FrameKey, FrameValue};

const MAX_DIMENSION: u32 = 4_096;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const FORWARD_REUSE_TICKS: i64 = 5_000_000;
const SCRUB_PROGRESS_INTERVAL: Duration = Duration::from_millis(8);
const POLL_INTERVAL: Duration = Duration::from_millis(8);
const LOW_LATENCY_DECODE_THREADS: usize = 2;
const MONITOR_WORKER_COUNT: usize = 4;
const SPARSE_CACHE_INTERVAL_TICKS: i64 = 250_000;
const SCRUB_CACHE_INTERVAL_TICKS: i64 = 50_000;
const SCRUB_CACHE_TOLERANCE_TICKS: i64 = 50_000;
const MAX_SCRUB_CACHE_INDEX_ENTRIES: usize = 1_024;
const MAX_CACHE_STREAM_STATES: usize = 4_096;
pub const DEFAULT_FRAME_CACHE_BYTES: usize = 1024 * 1024 * 1024;

/// A point-in-time view of bounded monitor decoder resource use.
///
/// Counts are runtime diagnostics only: they do not alter cache eviction or sticky-session
/// ownership policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorDecoderDiagnostics {
    pub frame_cache_capacity_bytes: usize,
    pub current_frame_cache_bytes: usize,
    pub peak_frame_cache_bytes: usize,
    pub active_sticky_sessions: usize,
    pub peak_sticky_sessions: usize,
    pub session_cap: usize,
}

struct DecoderResourceDiagnostics {
    frame_cache_capacity_bytes: usize,
    current_frame_cache_bytes: AtomicUsize,
    peak_frame_cache_bytes: AtomicUsize,
    active_sticky_session_mask: AtomicUsize,
    peak_sticky_sessions: AtomicUsize,
}

impl DecoderResourceDiagnostics {
    fn new(frame_cache_capacity_bytes: usize) -> Self {
        Self {
            frame_cache_capacity_bytes,
            current_frame_cache_bytes: AtomicUsize::new(0),
            peak_frame_cache_bytes: AtomicUsize::new(0),
            active_sticky_session_mask: AtomicUsize::new(0),
            peak_sticky_sessions: AtomicUsize::new(0),
        }
    }

    fn snapshot(&self) -> MonitorDecoderDiagnostics {
        let current_frame_cache_bytes = self.current_frame_cache_bytes.load(Ordering::Acquire);
        let active_sticky_sessions = self
            .active_sticky_session_mask
            .load(Ordering::Acquire)
            .count_ones() as usize;
        MonitorDecoderDiagnostics {
            frame_cache_capacity_bytes: self.frame_cache_capacity_bytes,
            current_frame_cache_bytes,
            // A reader may race publication across separate atomics. Clamp peak fields to the
            // observed current count so every individual snapshot remains internally coherent.
            peak_frame_cache_bytes: self
                .peak_frame_cache_bytes
                .load(Ordering::Acquire)
                .max(current_frame_cache_bytes),
            active_sticky_sessions,
            peak_sticky_sessions: self
                .peak_sticky_sessions
                .load(Ordering::Acquire)
                .max(active_sticky_sessions),
            session_cap: MONITOR_WORKER_COUNT,
        }
    }

    fn publish_cache_bytes(&self, used_bytes: usize) {
        debug_assert!(used_bytes <= self.frame_cache_capacity_bytes);
        self.peak_frame_cache_bytes
            .fetch_max(used_bytes, Ordering::AcqRel);
        self.current_frame_cache_bytes
            .store(used_bytes, Ordering::Release);
    }

    fn publish_worker_session(&self, worker_index: usize, active: bool) {
        debug_assert!(worker_index < MONITOR_WORKER_COUNT);
        let worker_bit = 1_usize << worker_index;
        let mask = if active {
            self.active_sticky_session_mask
                .fetch_or(worker_bit, Ordering::AcqRel)
                | worker_bit
        } else {
            self.active_sticky_session_mask
                .fetch_and(!worker_bit, Ordering::AcqRel)
                & !worker_bit
        };
        self.peak_sticky_sessions
            .fetch_max(mask.count_ones() as usize, Ordering::AcqRel);
    }
}

/// A fixed aggregate of completed CPU timing samples for one decoder-worker stage.
/// Durations use monotonic wall-clock time around the named CPU call boundary; they do not
/// represent GPU completion or display presentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorStageTiming {
    pub samples: u64,
    pub total_nanos: u64,
    pub max_nanos: u64,
}

impl MonitorStageTiming {
    pub fn total_ms(self) -> f64 {
        self.total_nanos as f64 / 1_000_000.0
    }

    pub fn mean_ms(self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.total_ms() / self.samples as f64
        }
    }

    pub fn max_ms(self) -> f64 {
        self.max_nanos as f64 / 1_000_000.0
    }

    pub fn merge(&mut self, other: Self) {
        self.samples = self.samples.saturating_add(other.samples);
        self.total_nanos = self.total_nanos.saturating_add(other.total_nanos);
        self.max_nanos = self.max_nanos.max(other.max_nanos);
    }
}

/// Runtime-only aggregate CPU timings shared by every bounded monitor-worker lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorDecoderStageTimings {
    pub cache_lookup: MonitorStageTiming,
    pub demux_packet: MonitorStageTiming,
    pub decoder_calls: MonitorStageTiming,
    pub hardware_transfer: MonitorStageTiming,
    pub scaler: MonitorStageTiming,
    pub rgba_copy_letterbox: MonitorStageTiming,
    pub worker_request: MonitorStageTiming,
}

impl MonitorDecoderStageTimings {
    pub fn merge(&mut self, other: Self) {
        self.cache_lookup.merge(other.cache_lookup);
        self.demux_packet.merge(other.demux_packet);
        self.decoder_calls.merge(other.decoder_calls);
        self.hardware_transfer.merge(other.hardware_transfer);
        self.scaler.merge(other.scaler);
        self.rgba_copy_letterbox.merge(other.rgba_copy_letterbox);
        self.worker_request.merge(other.worker_request);
    }
}

#[derive(Default)]
struct AtomicStageTiming {
    sequence: AtomicU64,
    samples: AtomicU64,
    total_nanos: AtomicU64,
    max_nanos: AtomicU64,
}

impl AtomicStageTiming {
    fn record(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        // Each accumulator belongs to one scheduler lane, so this is a single-writer sequence
        // lock. Two wrapping increments preserve odd/even publication until an impractical 2^63
        // recorded spans; readers retry whenever they race the writer.
        self.sequence.fetch_add(1, Ordering::AcqRel);
        let _ = self
            .samples
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |samples| {
                Some(samples.saturating_add(1))
            });
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
        let mut observed = self.total_nanos.load(Ordering::Relaxed);
        loop {
            let next = observed.saturating_add(nanos);
            match self.total_nanos.compare_exchange_weak(
                observed,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next_observed) => observed = next_observed,
            }
        }
        self.sequence.fetch_add(1, Ordering::Release);
    }

    fn snapshot(&self) -> MonitorStageTiming {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let snapshot = MonitorStageTiming {
                samples: self.samples.load(Ordering::Relaxed),
                total_nanos: self.total_nanos.load(Ordering::Relaxed),
                max_nanos: self.max_nanos.load(Ordering::Relaxed),
            };
            // Keep the data loads before the validating sequence load.
            fence(Ordering::Acquire);
            let after = self.sequence.load(Ordering::Relaxed);
            if before == after {
                return snapshot;
            }
            std::hint::spin_loop();
        }
    }
}

#[derive(Default)]
struct DecoderStageTimingAccumulators {
    cache_lookup: AtomicStageTiming,
    demux_packet: AtomicStageTiming,
    decoder_calls: AtomicStageTiming,
    hardware_transfer: AtomicStageTiming,
    scaler: AtomicStageTiming,
    rgba_copy_letterbox: AtomicStageTiming,
    worker_request: AtomicStageTiming,
}

impl DecoderStageTimingAccumulators {
    fn snapshot(&self) -> MonitorDecoderStageTimings {
        MonitorDecoderStageTimings {
            cache_lookup: self.cache_lookup.snapshot(),
            demux_packet: self.demux_packet.snapshot(),
            decoder_calls: self.decoder_calls.snapshot(),
            hardware_transfer: self.hardware_transfer.snapshot(),
            scaler: self.scaler.snapshot(),
            rgba_copy_letterbox: self.rgba_copy_letterbox.snapshot(),
            worker_request: self.worker_request.snapshot(),
        }
    }
}

struct StageTimer<'a> {
    stage: &'a AtomicStageTiming,
    started: Instant,
}

impl<'a> StageTimer<'a> {
    fn new(stage: &'a AtomicStageTiming) -> Self {
        Self {
            stage,
            started: Instant::now(),
        }
    }
}

impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.stage.record(self.started.elapsed());
    }
}

/// Limits progressive scrub publication by elapsed wall time, not media time.
///
/// Source timestamps vary by frame rate and can be irregular for VFR media, so
/// they must not determine how often the monitor receives a decoded frame.
fn scrub_progress_due(last_published: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last_published.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

/// Allows preroll only when it moves the visible monitor frame toward the target.
///
/// A forward seek may reveal frames after the current visible frame. A reverse (or
/// unanchored) seek must not reveal frames before its target: FFmpeg reaches those
/// by replaying from a keyframe, which makes the monitor jump in the wrong direction.
fn scrub_progress_moves_toward_target(
    last_visible_tick: Option<i64>,
    target_tick: i64,
    source_tick: i64,
) -> bool {
    match last_visible_tick {
        Some(last_visible_tick) if target_tick >= last_visible_tick => {
            source_tick >= last_visible_tick
        }
        Some(last_visible_tick) => source_tick >= target_tick && source_tick <= last_visible_tick,
        None => source_tick >= target_tick,
    }
}

/// Preferred acceleration policy for monitor decoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccelerationPreference {
    /// Use the platform's normal decoder selection.
    #[default]
    Auto,
    /// A hardware preference retained for caller compatibility.
    PreferHardware,
    /// A software preference retained for caller compatibility.
    Software,
}

/// One latest-wins request for a monitor frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeRequest {
    pub project_epoch: u64,
    /// Stable namespace for one open project's live frame cache.
    pub cache_epoch: u64,
    pub request_id: u64,
    pub media_id: u32,
    pub path: PathBuf,
    /// Source time in microseconds.
    pub source_tick: i64,
    pub width: u32,
    pub height: u32,
    /// True while the monitor is following a pointer drag rather than exact playback.
    pub is_scrubbing: bool,
    /// Warms the bounded random-access lane pool while the viewer is paused.
    pub prewarm_scrub_workers: bool,
    /// Selects the higher-quality FFmpeg scaling filter for the monitor raster.
    pub high_quality_scaling: bool,
    /// Publishes timed intermediate frames while a scrub traverses an inter-frame GOP.
    /// Frames between publication intervals remain decode-only and avoid scaling/copying work.
    pub progressive_scrub_frames: bool,
    /// Probed average source-frame duration. When known, scrub cache lookup never substitutes a
    /// frame more than one source frame after the requested timestamp.
    pub source_frame_duration_tick: Option<i64>,
    pub acceleration: AccelerationPreference,
}

/// A bounded, owned RGBA8 monitor frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedFrame {
    pub project_epoch: u64,
    pub request_id: u64,
    pub media_id: u32,
    pub source_tick: i64,
    pub width: u32,
    pub height: u32,
    /// Decoder that produced this frame. Cached frames retain the active source session's
    /// backend when one is available; callers should keep the last known concrete value.
    pub backend: Option<DecodeBackend>,
    pub rgba: Arc<[u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeBackend {
    Software,
    IntelQuickSync,
    Nvidia,
    VideoToolbox,
    D3D11VA,
    DXVA2,
}

impl DecodeBackend {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Software => "Software",
            Self::IntelQuickSync => "Intel Quick Sync",
            Self::Nvidia => "NVIDIA CUVID",
            Self::VideoToolbox => "Apple VideoToolbox",
            Self::D3D11VA => "Windows D3D11VA",
            Self::DXVA2 => "Windows DXVA2",
        }
    }
}

/// A request-specific decode failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub project_epoch: u64,
    pub request_id: u64,
    pub media_id: u32,
    pub source_tick: i64,
    pub message: String,
}

/// A completed monitor operation delivered without blocking the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeEvent {
    Frame(DecodedFrame),
    Error(DecodeError),
}

/// Returns normalized monitor dimensions and their exact RGBA allocation.
pub fn bounded_dimensions(width: u32, height: u32) -> (u32, u32, usize) {
    let mut width = width.clamp(1, MAX_DIMENSION);
    let mut height = height.clamp(1, MAX_DIMENSION);
    let max_pixels = MAX_FRAME_BYTES / 4;
    let pixels = u64::from(width) * u64::from(height);
    if pixels > max_pixels as u64 {
        let scale = ((max_pixels as f64) / (pixels as f64)).sqrt();
        width = ((width as f64 * scale).floor() as u32).clamp(1, MAX_DIMENSION);
        height = ((height as f64 * scale).floor() as u32).clamp(1, MAX_DIMENSION);
        while u64::from(width) * u64::from(height) > max_pixels as u64 {
            if width >= height && width > 1 {
                width -= 1;
            } else if height > 1 {
                height -= 1;
            } else {
                break;
            }
        }
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .expect("bounded dimensions fit usize");
    (width, height, bytes)
}

/// Latest-wins asynchronous monitor decoder.
///
/// A bounded scheduler pool owns independent libav input, decoder, and scaling contexts.
/// The primary lane preserves sequential playback, while reverse scrubs can use the other
/// lanes and their shared bounded frame cache. Each lane and the public event slot retain only
/// their latest work.
pub struct MonitorDecoder {
    workers: Vec<MonitorWorker>,
    events: Arc<EventSlot>,
    stage_timings: Vec<Arc<DecoderStageTimingAccumulators>>,
    last_scrub_target: Mutex<Option<(u32, i64)>>,
    cache_reset_generation: Arc<AtomicU64>,
    resource_diagnostics: Arc<DecoderResourceDiagnostics>,
}

struct MonitorWorker {
    commands: Arc<Mutex<Option<MonitorCommand>>>,
    wake: SyncSender<()>,
    scheduler: Option<JoinHandle<()>>,
}

impl MonitorDecoder {
    pub fn new() -> Self {
        Self::new_with_notifier(|| {})
    }

    /// Creates a decoder that wakes its owner whenever the one-slot event buffer changes.
    pub fn new_with_notifier(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self::new_with_notifier_and_cache_bytes(notify, DEFAULT_FRAME_CACHE_BYTES)
    }

    /// Creates a decoder whose worker owns a hard-capped decoded-frame cache.
    pub fn new_with_notifier_and_cache_bytes(
        notify: impl Fn() + Send + Sync + 'static,
        frame_cache_bytes: usize,
    ) -> Self {
        let events = Arc::new(EventSlot::new(notify));
        let cache_reset_generation = Arc::new(AtomicU64::new(0));
        let resource_diagnostics = Arc::new(DecoderResourceDiagnostics::new(frame_cache_bytes));
        let frame_cache = Arc::new(Mutex::new(MonitorFrameCache::new_with_diagnostics(
            frame_cache_bytes,
            Arc::clone(&resource_diagnostics),
        )));
        let mut workers = Vec::with_capacity(MONITOR_WORKER_COUNT);
        let mut stage_timings = Vec::with_capacity(MONITOR_WORKER_COUNT);
        for index in 0..MONITOR_WORKER_COUNT {
            let (wake, wake_rx) = mpsc::sync_channel(1);
            let commands = Arc::new(Mutex::new(None));
            let scheduler_commands = Arc::clone(&commands);
            let scheduler_events = Arc::clone(&events);
            let lane_stage_timings = Arc::new(DecoderStageTimingAccumulators::default());
            let scheduler_stage_timings = Arc::clone(&lane_stage_timings);
            let scheduler_cache_reset = Arc::clone(&cache_reset_generation);
            let scheduler_frame_cache = Arc::clone(&frame_cache);
            let scheduler_resource_diagnostics = Arc::clone(&resource_diagnostics);
            let scheduler = thread::Builder::new()
                .name(format!("maelstrom-monitor-decoder-{index}"))
                .spawn(move || {
                    monitor_scheduler_loop(
                        wake_rx,
                        scheduler_commands,
                        scheduler_events,
                        scheduler_stage_timings,
                        scheduler_cache_reset,
                        scheduler_frame_cache,
                        scheduler_resource_diagnostics,
                        index,
                    )
                })
                .expect("failed to start monitor decoder scheduler");
            workers.push(MonitorWorker {
                commands,
                wake,
                scheduler: Some(scheduler),
            });
            stage_timings.push(lane_stage_timings);
        }
        Self {
            workers,
            events,
            stage_timings,
            last_scrub_target: Mutex::new(None),
            cache_reset_generation,
            resource_diagnostics,
        }
    }

    /// Queues a target. Older queued targets are discarded.
    pub fn request(&self, request: DecodeRequest) -> Result<(), DecoderClosed> {
        // Ordinary playback stays on lane zero so its sticky session can decode sequentially.
        // Forward scrubbing shares that fast sequential lane. Reverse random access fans out
        // across a bounded lane pool so one slow GOP seek cannot block every newer pointer target.
        if !request.is_scrubbing {
            *self.last_scrub_target.lock().expect("scrub target lock") = None;
            if request.prewarm_scrub_workers {
                let mut closed = false;
                for index in 0..self.workers.len() {
                    let mut lane_request = request.clone();
                    lane_request.prewarm_scrub_workers = false;
                    closed |= self
                        .send_to(index, MonitorCommand::Request(lane_request))
                        .is_err();
                }
                return if closed { Err(DecoderClosed) } else { Ok(()) };
            }
            return self.send_to(0, MonitorCommand::Request(request));
        }
        let reverse = {
            let mut previous = self.last_scrub_target.lock().expect("scrub target lock");
            let reverse = previous.is_some_and(|(media_id, source_tick)| {
                media_id == request.media_id && request.source_tick < source_tick
            });
            *previous = Some((request.media_id, request.source_tick));
            reverse
        };
        let index = if reverse {
            request.request_id as usize % self.workers.len()
        } else {
            0
        };
        self.send_to(index, MonitorCommand::Request(request))
    }

    /// Clears queued work while retaining open media contexts for reuse.
    pub fn cancel_pending(&self) -> Result<(), DecoderClosed> {
        *self.last_scrub_target.lock().expect("scrub target lock") = None;
        let mut closed = false;
        for index in 0..self.workers.len() {
            closed |= self.send_to(index, MonitorCommand::Cancel).is_err();
        }
        if closed { Err(DecoderClosed) } else { Ok(()) }
    }

    /// Alias for [`Self::cancel_pending`].
    pub fn cancel(&self) -> Result<(), DecoderClosed> {
        self.cancel_pending()
    }

    /// Cancels active work and releases sticky decoder sessions and cached frames on workers.
    pub fn reset_live_cache(&self) -> Result<(), DecoderClosed> {
        self.cache_reset_generation.fetch_add(1, Ordering::AcqRel);
        self.cancel_pending()
    }

    /// Returns the newest completed result without blocking.
    pub fn try_recv(&self) -> Result<Option<DecodeEvent>, DecoderClosed> {
        Ok(self.events.take())
    }

    /// Returns fixed, runtime-only CPU timing aggregates across all worker lanes.
    pub fn stage_timings(&self) -> MonitorDecoderStageTimings {
        let mut aggregate = MonitorDecoderStageTimings::default();
        for lane in &self.stage_timings {
            aggregate.merge(lane.snapshot());
        }
        aggregate
    }

    /// Returns a copyable snapshot of bounded cache and sticky-session resource use.
    pub fn diagnostics(&self) -> MonitorDecoderDiagnostics {
        self.resource_diagnostics.snapshot()
    }

    fn send_to(&self, index: usize, command: MonitorCommand) -> Result<(), DecoderClosed> {
        let worker = &self.workers[index];
        *worker.commands.lock().expect("monitor command lock") = Some(command);
        match worker.wake.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(())) => Err(DecoderClosed),
        }
    }
}

impl Default for MonitorDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MonitorDecoder {
    fn drop(&mut self) {
        for index in 0..self.workers.len() {
            let _ = self.send_to(index, MonitorCommand::Shutdown);
        }
        for worker in &mut self.workers {
            if let Some(scheduler) = worker.scheduler.take() {
                let _ = scheduler.join();
            }
        }
    }
}

/// The scheduler has terminated and cannot accept or deliver work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecoderClosed;

impl fmt::Display for DecoderClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("monitor decoder scheduler has stopped")
    }
}

impl std::error::Error for DecoderClosed {}

struct EventSlot {
    state: Mutex<EventState>,
    notify: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct EventState {
    event: Option<DecodeEvent>,
    newest_request_id: u64,
}

impl EventSlot {
    fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            state: Mutex::new(EventState::default()),
            notify: Box::new(notify),
        }
    }

    fn publish(&self, event: DecodeEvent) {
        let request_id = match &event {
            DecodeEvent::Frame(frame) => frame.request_id,
            DecodeEvent::Error(error) => error.request_id,
        };
        let mut state = self.state.lock().expect("monitor event slot lock");
        if request_id < state.newest_request_id {
            return;
        }
        state.newest_request_id = request_id;
        state.event = Some(event);
        drop(state);
        (self.notify)();
    }

    fn take(&self) -> Option<DecodeEvent> {
        self.state
            .lock()
            .expect("monitor event slot lock")
            .event
            .take()
    }
}

enum MonitorCommand {
    Request(DecodeRequest),
    Cancel,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FrameStreamKey {
    project_epoch: u64,
    media_id: u32,
    width: u32,
    height: u32,
}

impl From<FrameKey> for FrameStreamKey {
    fn from(key: FrameKey) -> Self {
        Self {
            project_epoch: key.project_epoch,
            media_id: key.media_id,
            width: key.width,
            height: key.height,
        }
    }
}

/// Retains one sparse anchor per traversed time bucket plus the exact latest target.
/// Continuous scrubbing therefore cannot fill the LRU with every intermediate request.
struct MonitorFrameCache {
    frames: FrameCache,
    resource_diagnostics: Arc<DecoderResourceDiagnostics>,
    last_anchor_bucket: HashMap<FrameStreamKey, i64>,
    latest: HashMap<FrameStreamKey, (FrameKey, bool)>,
    /// Source-time lookup for scrub traversal frames. The frame cache remains the byte owner;
    /// this index is separately capped and removes stale keys as they are observed.
    scrub_frames: HashMap<FrameStreamKey, BTreeMap<i64, FrameKey>>,
    scrub_order: VecDeque<(FrameStreamKey, i64)>,
    project_epoch: Option<u64>,
    high_quality_scaling: Option<bool>,
    sources: HashMap<u32, PathBuf>,
}

impl MonitorFrameCache {
    #[cfg(test)]
    fn new(capacity_bytes: usize) -> Self {
        Self::new_with_diagnostics(
            capacity_bytes,
            Arc::new(DecoderResourceDiagnostics::new(capacity_bytes)),
        )
    }

    fn new_with_diagnostics(
        capacity_bytes: usize,
        resource_diagnostics: Arc<DecoderResourceDiagnostics>,
    ) -> Self {
        Self {
            frames: FrameCache::new(capacity_bytes),
            resource_diagnostics,
            last_anchor_bucket: HashMap::new(),
            latest: HashMap::new(),
            scrub_frames: HashMap::new(),
            scrub_order: VecDeque::new(),
            project_epoch: None,
            high_quality_scaling: None,
            sources: HashMap::new(),
        }
    }

    /// Returns true when all sticky decoder sessions must be retired too.
    fn prepare_request(&mut self, request: &DecodeRequest) -> bool {
        let stream = FrameStreamKey {
            project_epoch: request.cache_epoch,
            media_id: request.media_id,
            width: request.width,
            height: request.height,
        };
        let project_changed = self.project_epoch != Some(request.cache_epoch);
        // FrameKey is shared with the generic cache and intentionally does not carry scaler
        // policy. Clearing at a scaler-policy transition prevents either quality from reusing
        // pixels produced by the other.
        let scaling_changed = self.high_quality_scaling != Some(request.high_quality_scaling);
        let source_changed = self
            .sources
            .get(&request.media_id)
            .is_some_and(|path| path != &request.path);
        let stream_limit_reached = (!self.latest.contains_key(&stream)
            && self.latest.len() >= MAX_CACHE_STREAM_STATES)
            || (!self.sources.contains_key(&request.media_id)
                && self.sources.len() >= MAX_CACHE_STREAM_STATES);
        let reset_sessions =
            project_changed || source_changed || scaling_changed || stream_limit_reached;
        if reset_sessions {
            self.clear();
            self.project_epoch = Some(request.cache_epoch);
        }
        self.sources
            .entry(request.media_id)
            .or_insert_with(|| request.path.clone());
        self.high_quality_scaling = Some(request.high_quality_scaling);
        reset_sessions
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.publish_frame_cache_bytes();
        self.last_anchor_bucket.clear();
        self.latest.clear();
        self.scrub_frames.clear();
        self.scrub_order.clear();
        self.sources.clear();
        self.project_epoch = None;
        self.high_quality_scaling = None;
    }

    fn get(&mut self, key: &FrameKey) -> Option<FrameValue> {
        self.frames.get(key).cloned()
    }

    fn accepts_request(&self, request: &DecodeRequest) -> bool {
        self.project_epoch == Some(request.cache_epoch)
            && self.high_quality_scaling == Some(request.high_quality_scaling)
            && self.sources.get(&request.media_id) == Some(&request.path)
    }

    fn get_scrub_at_or_after(&mut self, request: &DecodeRequest) -> Option<FrameValue> {
        if !request.is_scrubbing {
            return None;
        }
        let stream = FrameStreamKey {
            project_epoch: request.cache_epoch,
            media_id: request.media_id,
            width: request.width,
            height: request.height,
        };
        let target = request.source_tick.max(0);
        let tolerance = request
            .source_frame_duration_tick
            .filter(|duration| *duration > 0)
            .unwrap_or(SCRUB_CACHE_TOLERANCE_TICKS);
        let upper = target.saturating_add(tolerance);
        let key = self
            .scrub_frames
            .get(&stream)
            .and_then(|frames| frames.range(target..=upper).next().map(|(_, key)| *key))?;
        match self.frames.get(&key).cloned() {
            Some(frame) if frame.source_tick >= target => Some(frame),
            _ => {
                if let Some(frames) = self.scrub_frames.get_mut(&stream) {
                    frames.remove(&key.source_tick);
                    if frames.is_empty() {
                        self.scrub_frames.remove(&stream);
                    }
                }
                None
            }
        }
    }

    fn insert_scrub_traversal(&mut self, request: &DecodeRequest, frame: &DecodedRgba) {
        if !request.is_scrubbing || !self.accepts_request(request) {
            return;
        }
        let stream = FrameStreamKey {
            project_epoch: request.cache_epoch,
            media_id: request.media_id,
            width: request.width,
            height: request.height,
        };
        let source_tick = frame.source_tick.max(0);
        let interval = request
            .source_frame_duration_tick
            .filter(|duration| *duration > 0)
            .map(|duration| (duration / 2).max(1))
            .unwrap_or(SCRUB_CACHE_INTERVAL_TICKS);
        let bucket_start = source_tick.div_euclid(interval) * interval;
        let bucket_end = bucket_start.saturating_add(interval - 1);
        if self
            .scrub_frames
            .get(&stream)
            .is_some_and(|frames| frames.range(bucket_start..=bucket_end).next().is_some())
        {
            return;
        }
        let key = frame_cache_key(request, source_tick);
        if !self.frames.insert(
            key,
            FrameValue::new(
                frame.source_tick,
                frame.width,
                frame.height,
                Arc::clone(&frame.rgba),
            ),
        ) {
            self.publish_frame_cache_bytes();
            return;
        }
        self.publish_frame_cache_bytes();
        self.scrub_frames
            .entry(stream)
            .or_default()
            .insert(source_tick, key);
        self.scrub_order.push_back((stream, source_tick));
        while self.scrub_order.len() > MAX_SCRUB_CACHE_INDEX_ENTRIES {
            let Some((old_stream, old_tick)) = self.scrub_order.pop_front() else {
                break;
            };
            if let Some(frames) = self.scrub_frames.get_mut(&old_stream) {
                frames.remove(&old_tick);
                if frames.is_empty() {
                    self.scrub_frames.remove(&old_stream);
                }
            }
        }
    }

    fn insert(&mut self, key: FrameKey, value: FrameValue) -> bool {
        let stream = FrameStreamKey::from(key);
        let bucket = key.source_tick.div_euclid(SPARSE_CACHE_INTERVAL_TICKS);
        let is_anchor = self.last_anchor_bucket.get(&stream).copied() != Some(bucket);
        if !self.frames.insert(key, value) {
            self.publish_frame_cache_bytes();
            return false;
        }
        self.publish_frame_cache_bytes();
        if is_anchor {
            self.last_anchor_bucket.insert(stream, bucket);
        }
        if let Some((previous, previous_is_anchor)) = self.latest.insert(stream, (key, is_anchor))
            && previous != key
            && !previous_is_anchor
        {
            let retained_for_scrub = self
                .scrub_frames
                .get(&stream)
                .is_some_and(|frames| frames.values().any(|scrub_key| *scrub_key == previous));
            if !retained_for_scrub {
                let _ = self.frames.remove(&previous);
                self.publish_frame_cache_bytes();
            }
        }
        true
    }

    fn insert_if_current(
        &mut self,
        request: &DecodeRequest,
        key: FrameKey,
        value: FrameValue,
    ) -> bool {
        self.accepts_request(request) && self.insert(key, value)
    }

    fn publish_frame_cache_bytes(&self) {
        self.resource_diagnostics
            .publish_cache_bytes(self.frames.used_bytes());
    }
}

fn monitor_scheduler_loop(
    wake: Receiver<()>,
    commands: Arc<Mutex<Option<MonitorCommand>>>,
    events: Arc<EventSlot>,
    stage_timings: Arc<DecoderStageTimingAccumulators>,
    cache_reset_generation: Arc<AtomicU64>,
    frame_cache: Arc<Mutex<MonitorFrameCache>>,
    resource_diagnostics: Arc<DecoderResourceDiagnostics>,
    worker_index: usize,
) {
    if ffmpeg::init().is_err() {
        return;
    }
    let mut sessions = HashMap::<u32, StickyMonitor>::new();
    let mut observed_cache_reset = cache_reset_generation.load(Ordering::Acquire);
    let mut pending = None;
    loop {
        let requested_cache_reset = cache_reset_generation.load(Ordering::Acquire);
        if requested_cache_reset != observed_cache_reset {
            sessions.clear();
            resource_diagnostics.publish_worker_session(worker_index, false);
            frame_cache
                .lock()
                .expect("monitor frame cache lock")
                .clear();
            observed_cache_reset = requested_cache_reset;
        }
        if pending.is_none() {
            match wake.recv_timeout(POLL_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    sessions.clear();
                    resource_diagnostics.publish_worker_session(worker_index, false);
                    return;
                }
            }
            match commands.lock().expect("monitor command lock").take() {
                Some(MonitorCommand::Request(request)) => pending = Some(request),
                Some(MonitorCommand::Cancel) | None => continue,
                Some(MonitorCommand::Shutdown) => {
                    sessions.clear();
                    resource_diagnostics.publish_worker_session(worker_index, false);
                    return;
                }
            }
        }

        let request = pending.take().expect("pending monitor request");
        let _request_timer = StageTimer::new(&stage_timings.worker_request);
        let progress_events = Arc::clone(&events);
        let progress_backend = sessions
            .get(&request.media_id)
            .map(|session| session.backend);
        let mut on_progress = |frame: &DecodedRgba| {
            progress_events.publish(DecodeEvent::Frame(DecodedFrame {
                project_epoch: request.project_epoch,
                request_id: frame.request_id,
                media_id: request.media_id,
                source_tick: frame.source_tick,
                width: frame.width,
                height: frame.height,
                backend: progress_backend,
                rgba: Arc::clone(&frame.rgba),
            }));
        };
        let mut on_session_state = |active| {
            resource_diagnostics.publish_worker_session(worker_index, active);
        };
        let event = decode_monitor_request(
            &mut sessions,
            &frame_cache,
            &request,
            &commands,
            &mut on_progress,
            &mut on_session_state,
            &stage_timings,
        );
        // A target arriving during decode wins: do not publish old output.
        match commands.lock().expect("monitor command lock").take() {
            Some(MonitorCommand::Request(newer)) => {
                let completed_newest = event
                    .as_ref()
                    .is_some_and(|event| event_request_id(event) == newer.request_id);
                if same_decode_generation(&request, &newer) {
                    if let Some(event) = event {
                        events.publish(event);
                    }
                    if !completed_newest {
                        pending = Some(newer);
                    }
                } else {
                    pending = Some(newer);
                }
            }
            Some(MonitorCommand::Cancel) => {}
            Some(MonitorCommand::Shutdown) => {
                sessions.clear();
                resource_diagnostics.publish_worker_session(worker_index, false);
                return;
            }
            None => {
                if let Some(event) = event {
                    events.publish(event);
                }
            }
        }
    }
}

fn event_request_id(event: &DecodeEvent) -> u64 {
    match event {
        DecodeEvent::Frame(frame) => frame.request_id,
        DecodeEvent::Error(error) => error.request_id,
    }
}

fn same_decode_generation(left: &DecodeRequest, right: &DecodeRequest) -> bool {
    left.project_epoch == right.project_epoch
        && left.media_id == right.media_id
        && left.path == right.path
        && left.width == right.width
        && left.height == right.height
        && left.is_scrubbing == right.is_scrubbing
        && left.prewarm_scrub_workers == right.prewarm_scrub_workers
        && left.high_quality_scaling == right.high_quality_scaling
        && left.progressive_scrub_frames == right.progressive_scrub_frames
        && left.source_frame_duration_tick == right.source_frame_duration_tick
        && left.acceleration == right.acceleration
}

fn decode_monitor_request(
    sessions: &mut HashMap<u32, StickyMonitor>,
    frame_cache: &Arc<Mutex<MonitorFrameCache>>,
    request: &DecodeRequest,
    commands: &Arc<Mutex<Option<MonitorCommand>>>,
    on_progress: &mut dyn FnMut(&DecodedRgba),
    on_session_state: &mut dyn FnMut(bool),
    stage_timings: &DecoderStageTimingAccumulators,
) -> Option<DecodeEvent> {
    let span = tracing::debug_span!(
        "monitor_decode",
        media_id = request.media_id,
        request_id = request.request_id,
        source_tick = request.source_tick,
        width = request.width,
        height = request.height,
        acceleration = ?request.acceleration,
    );
    let _entered = span.enter();
    if frame_cache
        .lock()
        .expect("monitor frame cache lock")
        .prepare_request(request)
    {
        sessions.clear();
        on_session_state(false);
    }
    // An application monitor slot owns exactly one active source, and each MonitorDecoder owns
    // one worker. Keep a same-source FFmpeg context sticky for seeks, but release every inactive
    // source before a new request can reuse/open a session. This bounds per-slot session retention
    // to one even when timeline layering switches media repeatedly.
    retain_active_monitor_session(sessions, request.media_id);
    on_session_state(!sessions.is_empty());
    let cache_key = frame_cache_key(request, request.source_tick);
    let cached = {
        let _timer = StageTimer::new(&stage_timings.cache_lookup);
        frame_cache
            .lock()
            .expect("monitor frame cache lock")
            .get(&cache_key)
    };
    if let Some(cached) = cached {
        return Some(DecodeEvent::Frame(DecodedFrame {
            project_epoch: request.project_epoch,
            request_id: request.request_id,
            media_id: request.media_id,
            source_tick: cached.source_tick,
            width: cached.width,
            height: cached.height,
            backend: sessions
                .get(&request.media_id)
                .map(|session| session.backend),
            rgba: cached.rgba,
        }));
    }
    let cached = {
        let _timer = StageTimer::new(&stage_timings.cache_lookup);
        frame_cache
            .lock()
            .expect("monitor frame cache lock")
            .get_scrub_at_or_after(request)
    };
    if let Some(cached) = cached {
        return Some(DecodeEvent::Frame(DecodedFrame {
            project_epoch: request.project_epoch,
            request_id: request.request_id,
            media_id: request.media_id,
            source_tick: cached.source_tick,
            width: cached.width,
            height: cached.height,
            backend: sessions
                .get(&request.media_id)
                .map(|session| session.backend),
            rgba: cached.rgba,
        }));
    }
    let mut on_traversal = |frame: &DecodedRgba| {
        frame_cache
            .lock()
            .expect("monitor frame cache lock")
            .insert_scrub_traversal(request, frame)
    };
    let decoded = match sessions.get_mut(&request.media_id) {
        Some(session) if session.path == request.path => decode_with_session(
            session,
            request,
            commands,
            on_progress,
            &mut on_traversal,
            stage_timings,
        ),
        _ => {
            sessions.remove(&request.media_id);
            on_session_state(!sessions.is_empty());
            match StickyMonitor::open(request) {
                Ok(mut session) => {
                    let result = decode_with_session(
                        &mut session,
                        request,
                        commands,
                        on_progress,
                        &mut on_traversal,
                        stage_timings,
                    );
                    sessions.insert(request.media_id, session);
                    on_session_state(true);
                    result
                }
                Err(error) => Err(error),
            }
        }
    };
    let decoded = match decoded {
        Err(hardware_error) => recover_hardware_decode_failure(
            sessions,
            request,
            commands,
            hardware_error,
            on_progress,
            &mut on_traversal,
            on_session_state,
            stage_timings,
        ),
        result => result,
    };
    match decoded {
        Ok(Some(frame)) => {
            let backend = sessions
                .get(&request.media_id)
                .map(|session| session.backend);
            let decoded = DecodedFrame {
                project_epoch: request.project_epoch,
                request_id: frame.request_id,
                media_id: request.media_id,
                source_tick: frame.source_tick,
                width: frame.width,
                height: frame.height,
                backend,
                rgba: Arc::clone(&frame.rgba),
            };
            let _ = frame_cache
                .lock()
                .expect("monitor frame cache lock")
                .insert_if_current(
                    request,
                    frame_cache_key(request, frame.target_tick),
                    FrameValue::new(
                        frame.source_tick,
                        frame.width,
                        frame.height,
                        Arc::clone(&frame.rgba),
                    ),
                );
            Some(DecodeEvent::Frame(decoded))
        }
        Ok(None) => None,
        Err(message) => Some(DecodeEvent::Error(DecodeError {
            project_epoch: request.project_epoch,
            request_id: request.request_id,
            media_id: request.media_id,
            source_tick: request.source_tick,
            message,
        })),
    }
}

fn retain_active_monitor_session<T>(sessions: &mut HashMap<u32, T>, active_media_id: u32) {
    sessions.retain(|media_id, _| *media_id == active_media_id);
}

fn decode_with_session(
    session: &mut StickyMonitor,
    request: &DecodeRequest,
    commands: &Arc<Mutex<Option<MonitorCommand>>>,
    on_progress: &mut dyn FnMut(&DecodedRgba),
    on_traversal: &mut dyn FnMut(&DecodedRgba),
    stage_timings: &DecoderStageTimingAccumulators,
) -> Result<Option<DecodedRgba>, String> {
    session.decode(
        request,
        || {
            matches!(
                commands.lock().expect("monitor command lock").as_ref(),
                Some(MonitorCommand::Cancel | MonitorCommand::Shutdown)
            ) || matches!(
                commands.lock().expect("monitor command lock").as_ref(),
                Some(MonitorCommand::Request(newer)) if !same_decode_generation(request, newer)
            )
        },
        || latest_same_generation(commands, request),
        on_progress,
        on_traversal,
        stage_timings,
    )
}

fn recover_hardware_decode_failure(
    sessions: &mut HashMap<u32, StickyMonitor>,
    request: &DecodeRequest,
    commands: &Arc<Mutex<Option<MonitorCommand>>>,
    hardware_error: String,
    on_progress: &mut dyn FnMut(&DecodedRgba),
    on_traversal: &mut dyn FnMut(&DecodedRgba),
    on_session_state: &mut dyn FnMut(bool),
    stage_timings: &DecoderStageTimingAccumulators,
) -> Result<Option<DecodedRgba>, String> {
    let fallback = sessions
        .get(&request.media_id)
        .and_then(|session| software_fallback_request(request, session.backend));
    let Some(open_request) = fallback else {
        return Err(hardware_error);
    };
    sessions.remove(&request.media_id);
    on_session_state(!sessions.is_empty());
    match StickyMonitor::open(&open_request) {
        Ok(mut session) => {
            tracing::warn!(
                target: "maelstrom::decode",
                media_id = request.media_id,
                error = %hardware_error,
                "hardware decoder failed during decode; retaining a software session"
            );
            // Decode the caller's original generation. Only the open policy changes;
            // cancellation/coalescing must still compare against PreferHardware.
            let result = decode_with_session(
                &mut session,
                request,
                commands,
                on_progress,
                on_traversal,
                stage_timings,
            ).map_err(
                |software_error| {
                    format!(
                        "hardware decoder failed ({hardware_error}); software fallback failed ({software_error})"
                    )
                },
            );
            sessions.insert(request.media_id, session);
            on_session_state(true);
            result
        }
        Err(software_error) => Err(format!(
            "hardware decoder failed ({hardware_error}); software fallback could not open ({software_error})"
        )),
    }
}

fn software_fallback_request(
    request: &DecodeRequest,
    failed_backend: DecodeBackend,
) -> Option<DecodeRequest> {
    (failed_backend != DecodeBackend::Software
        && request.acceleration != AccelerationPreference::Software)
        .then(|| {
            let mut fallback = request.clone();
            fallback.acceleration = AccelerationPreference::Software;
            fallback
        })
}

fn frame_cache_key(request: &DecodeRequest, target_tick: i64) -> FrameKey {
    FrameKey {
        project_epoch: request.cache_epoch,
        media_id: request.media_id,
        source_tick: target_tick.max(0),
        width: request.width,
        height: request.height,
    }
}

fn latest_same_generation(
    commands: &Arc<Mutex<Option<MonitorCommand>>>,
    request: &DecodeRequest,
) -> Option<DecodeRequest> {
    match commands.lock().expect("monitor command lock").as_ref() {
        Some(MonitorCommand::Request(newer)) if same_decode_generation(request, newer) => {
            Some(newer.clone())
        }
        _ => None,
    }
}

struct StickyMonitor {
    path: PathBuf,
    input: ffmpeg::format::context::Input,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    decoder: ffmpeg::decoder::Video,
    scaler: Option<ScalingContext>,
    scaler_input: Option<(Pixel, u32, u32)>,
    scaler_high_quality: Option<bool>,
    output_size: (u32, u32),
    scaled_size: (u32, u32),
    last_source_tick: Option<i64>,
    last_visible_tick: Option<i64>,
    backend: DecodeBackend,
    transfer_hardware_frames: bool,
}

struct DecodedRgba {
    request_id: u64,
    target_tick: i64,
    source_tick: i64,
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl StickyMonitor {
    fn open(request: &DecodeRequest) -> Result<Self, String> {
        let input = ffmpeg::format::input(&request.path)
            .map_err(|error| format!("could not open monitor media: {error}"))?;
        let stream = input
            .streams()
            .best(Type::Video)
            .ok_or_else(|| "monitor media has no video stream".to_owned())?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let (decoder, backend) = open_video_decoder(&stream, request.acceleration)?;
        tracing::info!(
            target: "maelstrom::decode",
            media_id = request.media_id,
            backend = backend.display_name(),
            path = %request.path.display(),
            "opened sticky monitor decoder"
        );
        let (width, height, _) = bounded_dimensions(request.width, request.height);
        let transfer_hardware_frames = requires_cpu_frame_transfer(backend);
        let scaler_input = (!transfer_hardware_frames)
            .then(|| (decoder.format(), decoder.width(), decoder.height()));
        let (scaler, scaled_size) = match scaler_input {
            Some((format, source_width, source_height)) => {
                let (scaler, scaled_size) = Self::make_scaler(
                    format,
                    source_width,
                    source_height,
                    width,
                    height,
                    request.high_quality_scaling,
                )?;
                (Some(scaler), scaled_size)
            }
            None => (
                None,
                fitted_size(decoder.width(), decoder.height(), width, height),
            ),
        };
        Ok(Self {
            path: request.path.clone(),
            input,
            stream_index,
            time_base,
            decoder,
            scaler,
            scaler_input,
            scaler_high_quality: Some(request.high_quality_scaling),
            output_size: (width, height),
            scaled_size,
            last_source_tick: None,
            last_visible_tick: None,
            backend,
            transfer_hardware_frames,
        })
    }

    fn make_scaler(
        format: Pixel,
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
        high_quality_scaling: bool,
    ) -> Result<(ScalingContext, (u32, u32)), String> {
        let scaled_size = fitted_size(source_width, source_height, width, height);
        let scaler = ScalingContext::get(
            format,
            source_width,
            source_height,
            Pixel::RGBA,
            scaled_size.0,
            scaled_size.1,
            scaling_flags(high_quality_scaling),
        )
        .map_err(|error| format!("could not create RGBA scaler: {error}"))?;
        Ok((scaler, scaled_size))
    }

    fn decode(
        &mut self,
        request: &DecodeRequest,
        mut invalidated: impl FnMut() -> bool,
        mut newest_target: impl FnMut() -> Option<DecodeRequest>,
        on_progress: &mut dyn FnMut(&DecodedRgba),
        on_traversal: &mut dyn FnMut(&DecodedRgba),
        stage_timings: &DecoderStageTimingAccumulators,
    ) -> Result<Option<DecodedRgba>, String> {
        let (width, height, _) = bounded_dimensions(request.width, request.height);
        if self.output_size != (width, height)
            || self.scaler_high_quality != Some(request.high_quality_scaling)
        {
            if let Some((format, source_width, source_height)) = self.scaler_input {
                let (scaler, scaled_size) = Self::make_scaler(
                    format,
                    source_width,
                    source_height,
                    width,
                    height,
                    request.high_quality_scaling,
                )?;
                self.scaler = Some(scaler);
                self.scaled_size = scaled_size;
            } else {
                self.scaled_size =
                    fitted_size(self.decoder.width(), self.decoder.height(), width, height);
            }
            self.output_size = (width, height);
            self.scaler_high_quality = Some(request.high_quality_scaling);
        }
        let mut target = request.source_tick.max(0);
        let mut progress_target = target;
        let mut completed_request_id = request.request_id;
        // Coalesce a request that was already queued before this seek begins. Once packet
        // traversal starts, reverse arrivals are left for the scheduler's next seek so a
        // continuous reverse drag still receives progressive frames instead of starvation.
        if let Some(newer) = newest_target() {
            target = newer.source_tick.max(0);
            progress_target = target;
            completed_request_id = newer.request_id;
        }
        let dense_reverse_cache = self
            .last_source_tick
            .is_some_and(|last_source_tick| target < last_source_tick);
        let can_continue = self.last_source_tick.is_some_and(|last| {
            target > last && target.saturating_sub(last) <= FORWARD_REUSE_TICKS
        });
        if !can_continue {
            let target_ts = target.rescale((1, 1_000_000), self.time_base);
            self.input
                .seek(target_ts, ..target_ts)
                .map_err(|error| format!("could not seek monitor media: {error}"))?;
            {
                let _timer = StageTimer::new(&stage_timings.decoder_calls);
                self.decoder.flush();
            }
            self.last_source_tick = None;
        }

        let stream_index = self.stream_index;
        let time_base = self.time_base;
        let transfer_hardware_frames = self.transfer_hardware_frames;
        let output_size = self.output_size;
        let decoder = &mut self.decoder;
        let scaler = &mut self.scaler;
        let scaler_input = &mut self.scaler_input;
        let scaler_high_quality = &mut self.scaler_high_quality;
        let scaled_size = &mut self.scaled_size;
        let mut last_tick = self.last_source_tick;
        let mut last_progress_published = None;
        let mut packets = self.input.packets();
        while let Some((stream, packet)) = {
            let _timer = StageTimer::new(&stage_timings.demux_packet);
            packets.next()
        } {
            if invalidated() {
                self.last_source_tick = None;
                return Ok(None);
            }
            // Scrub input can move several times before a long GOP yields a frame. Retarget
            // between packets when the open preroll can reach the newer point. A reverse target
            // needs a new backward seek; abandoning every in-flight preroll for it starves the
            // monitor during continuous reverse motion. Finish one accurate progressive frame,
            // then let the scheduler seek to its retained newest request.
            if let Some(newer) = newest_target() {
                let newer_target = newer.source_tick.max(0);
                progress_target = newer_target;
                if newer_target >= target {
                    target = newer_target;
                    completed_request_id = newer.request_id;
                }
            }
            if stream.index() != stream_index {
                continue;
            }
            {
                let _timer = StageTimer::new(&stage_timings.decoder_calls);
                decoder
                    .send_packet(&packet)
                    .map_err(|error| format!("could not send video packet: {error}"))?;
            }
            let mut decoded = Video::empty();
            while {
                let _timer = StageTimer::new(&stage_timings.decoder_calls);
                decoder.receive_frame(&mut decoded).is_ok()
            } {
                if invalidated() {
                    self.last_source_tick = None;
                    return Ok(None);
                }
                let source_tick = decoded
                    .timestamp()
                    .or_else(|| decoded.pts())
                    .map(|timestamp| timestamp.rescale(time_base, (1, 1_000_000)))
                    .unwrap_or(target);
                if let Some(newer) = newest_target() {
                    let newer_target = newer.source_tick.max(0);
                    progress_target = newer_target;
                    // The open preroll can satisfy either a later target or an earlier target
                    // that is still ahead of the frame just decoded. This lets a reverse drag
                    // coalesce within the current GOP instead of finishing obsolete work or
                    // restarting on every pointer event.
                    if newer_target >= source_tick {
                        target = newer_target;
                        completed_request_id = newer.request_id;
                    }
                }
                if source_tick < target {
                    let now = Instant::now();
                    let publish_progress = request.progressive_scrub_frames
                        && scrub_progress_moves_toward_target(
                            self.last_visible_tick,
                            progress_target,
                            source_tick,
                        )
                        && scrub_progress_due(
                            last_progress_published,
                            now,
                            SCRUB_PROGRESS_INTERVAL,
                        );
                    if publish_progress || (request.is_scrubbing && dense_reverse_cache) {
                        let frame = pack_decoded_monitor_frame(
                            scaler,
                            scaler_input,
                            scaler_high_quality,
                            scaled_size,
                            &decoded,
                            transfer_hardware_frames,
                            output_size,
                            request.high_quality_scaling,
                            completed_request_id,
                            target,
                            source_tick,
                            stage_timings,
                        )?;
                        on_traversal(&frame);
                        if publish_progress {
                            last_tick = Some(source_tick);
                            self.last_visible_tick = last_tick;
                            last_progress_published = Some(now);
                            on_progress(&frame);
                        }
                    }
                    continue;
                }
                let frame = pack_decoded_monitor_frame(
                    scaler,
                    scaler_input,
                    scaler_high_quality,
                    scaled_size,
                    &decoded,
                    transfer_hardware_frames,
                    output_size,
                    request.high_quality_scaling,
                    completed_request_id,
                    target,
                    source_tick,
                    stage_timings,
                )?;
                last_tick = Some(source_tick);
                self.last_visible_tick = last_tick;
                self.last_source_tick = last_tick;
                return Ok(Some(frame));
            }
        }
        {
            let _timer = StageTimer::new(&stage_timings.decoder_calls);
            decoder
                .send_eof()
                .map_err(|error| format!("could not flush video decoder: {error}"))?;
        }
        let mut decoded = Video::empty();
        while {
            let _timer = StageTimer::new(&stage_timings.decoder_calls);
            decoder.receive_frame(&mut decoded).is_ok()
        } {
            if invalidated() {
                self.last_source_tick = None;
                return Ok(None);
            }
            let source_tick = decoded
                .timestamp()
                .or_else(|| decoded.pts())
                .map(|timestamp| timestamp.rescale(time_base, (1, 1_000_000)))
                .unwrap_or(target);
            if let Some(newer) = newest_target() {
                let newer_target = newer.source_tick.max(0);
                progress_target = newer_target;
                if newer_target >= source_tick {
                    target = newer_target;
                    completed_request_id = newer.request_id;
                }
            }
            if source_tick < target {
                let now = Instant::now();
                let publish_progress = request.progressive_scrub_frames
                    && scrub_progress_moves_toward_target(
                        self.last_visible_tick,
                        progress_target,
                        source_tick,
                    )
                    && scrub_progress_due(last_progress_published, now, SCRUB_PROGRESS_INTERVAL);
                if publish_progress || (request.is_scrubbing && dense_reverse_cache) {
                    let frame = pack_decoded_monitor_frame(
                        scaler,
                        scaler_input,
                        scaler_high_quality,
                        scaled_size,
                        &decoded,
                        transfer_hardware_frames,
                        output_size,
                        request.high_quality_scaling,
                        completed_request_id,
                        target,
                        source_tick,
                        stage_timings,
                    )?;
                    on_traversal(&frame);
                    if publish_progress {
                        last_tick = Some(source_tick);
                        self.last_visible_tick = last_tick;
                        last_progress_published = Some(now);
                        on_progress(&frame);
                    }
                }
                continue;
            }
            let frame = pack_decoded_monitor_frame(
                scaler,
                scaler_input,
                scaler_high_quality,
                scaled_size,
                &decoded,
                transfer_hardware_frames,
                output_size,
                request.high_quality_scaling,
                completed_request_id,
                target,
                source_tick,
                stage_timings,
            )?;
            last_tick = Some(source_tick);
            self.last_visible_tick = last_tick;
            self.last_source_tick = last_tick;
            return Ok(Some(frame));
        }
        self.last_source_tick = last_tick;
        Err("monitor decoder reached end of video before target".to_owned())
    }
}

fn pack_decoded_monitor_frame(
    scaler: &mut Option<ScalingContext>,
    scaler_input: &mut Option<(Pixel, u32, u32)>,
    scaler_high_quality: &mut Option<bool>,
    scaled_size: &mut (u32, u32),
    decoded: &Video,
    transfer_hardware_frames: bool,
    output_size: (u32, u32),
    high_quality_scaling: bool,
    request_id: u64,
    target_tick: i64,
    source_tick: i64,
    stage_timings: &DecoderStageTimingAccumulators,
) -> Result<DecodedRgba, String> {
    let rgba_frame = scale_monitor_frame(
        scaler,
        scaler_input,
        scaler_high_quality,
        scaled_size,
        decoded,
        transfer_hardware_frames,
        output_size,
        high_quality_scaling,
        stage_timings,
    )?;
    let rgba = {
        let _timer = StageTimer::new(&stage_timings.rgba_copy_letterbox);
        let scaled = copy_rgba_frame(&rgba_frame, scaled_size.0, scaled_size.1)?;
        letterbox_rgba(scaled, *scaled_size, output_size)
    };
    Ok(DecodedRgba {
        request_id,
        target_tick,
        source_tick,
        width: output_size.0,
        height: output_size.1,
        rgba: Arc::from(rgba),
    })
}

fn open_video_decoder(
    stream: &ffmpeg::format::stream::Stream<'_>,
    acceleration: AccelerationPreference,
) -> Result<(ffmpeg::decoder::Video, DecodeBackend), String> {
    #[cfg(target_os = "macos")]
    if acceleration != AccelerationPreference::Software
        && let Ok(video) = open_videotoolbox_decoder(stream)
    {
        return Ok((video, DecodeBackend::VideoToolbox));
    }

    let codec_id = stream.parameters().id();
    if acceleration != AccelerationPreference::Software {
        // Prefer FFmpeg's native Windows hardware context. It keeps the standard decoder's seek
        // semantics while still decoding on the selected D3D11 device; named CUVID/QSV decoders
        // remain fallbacks for codecs or drivers without a usable D3D11VA/DXVA2 configuration.
        #[cfg(target_os = "windows")]
        if let Ok((video, backend)) = open_windows_hardware_decoder(stream) {
            return Ok((video, backend));
        }

        for &(name, backend) in hardware_decoder_candidates(codec_id) {
            let Some(codec) = ffmpeg::decoder::find_by_name(name) else {
                continue;
            };
            let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
                .map_err(|error| format!("could not create video decoder: {error}"))?;
            if let Ok(opened) = context.decoder().open_as(codec)
                && let Ok(video) = opened.video()
            {
                return Ok((video, backend));
            }
        }
    }

    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|error| format!("could not create video decoder: {error}"))?;
    context.set_threading(ffmpeg::threading::Config {
        // Keep software frame parallelism bounded. Large frame-thread pools add their entire
        // queue as scrub latency before returning the first decoded frame.
        kind: ffmpeg::threading::Type::Frame,
        count: LOW_LATENCY_DECODE_THREADS,
    });
    context
        .decoder()
        .video()
        .map(|video| (video, DecodeBackend::Software))
        .map_err(|error| format!("could not open video decoder: {error}"))
}

#[cfg(target_os = "windows")]
fn open_windows_hardware_decoder(
    stream: &ffmpeg::format::stream::Stream<'_>,
) -> Result<(ffmpeg::decoder::Video, DecodeBackend), String> {
    let codec_id = stream.parameters().id();
    let codec = ffmpeg::decoder::find(codec_id)
        .ok_or_else(|| format!("could not find video decoder for {codec_id:?}"))?;
    for candidate in windows_hardware_decoder_candidates() {
        if !codec_supports_hardware_config(&codec, candidate.device_type, candidate.pixel_format) {
            continue;
        }
        if let Ok(video) = open_hardware_device_decoder(
            stream,
            codec,
            candidate.device_type,
            candidate.select_format,
        ) {
            return Ok((video, candidate.backend));
        }
    }
    Err("video decoder has no usable Windows hardware configuration".to_owned())
}

#[cfg(target_os = "windows")]
struct WindowsHardwareDecoderCandidate {
    backend: DecodeBackend,
    device_type: ffmpeg::ffi::AVHWDeviceType,
    pixel_format: ffmpeg::ffi::AVPixelFormat,
    select_format: unsafe extern "C" fn(
        *mut ffmpeg::ffi::AVCodecContext,
        *const ffmpeg::ffi::AVPixelFormat,
    ) -> ffmpeg::ffi::AVPixelFormat,
}

#[cfg(target_os = "windows")]
fn windows_hardware_decoder_candidates() -> [WindowsHardwareDecoderCandidate; 2] {
    [
        WindowsHardwareDecoderCandidate {
            backend: DecodeBackend::D3D11VA,
            device_type: ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            pixel_format: ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            select_format: select_d3d11va_format,
        },
        WindowsHardwareDecoderCandidate {
            backend: DecodeBackend::DXVA2,
            device_type: ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2,
            pixel_format: ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_DXVA2_VLD,
            select_format: select_dxva2_format,
        },
    ]
}

#[cfg(target_os = "windows")]
fn codec_supports_hardware_config(
    codec: &ffmpeg::Codec,
    device_type: ffmpeg::ffi::AVHWDeviceType,
    pixel_format: ffmpeg::ffi::AVPixelFormat,
) -> bool {
    let mut index = 0;
    loop {
        let config = unsafe { ffmpeg::ffi::avcodec_get_hw_config(codec.as_ptr(), index) };
        if config.is_null() {
            return false;
        }
        let config = unsafe { &*config };
        if hardware_config_uses_device_context(config, device_type, pixel_format) {
            return true;
        }
        index += 1;
    }
}

#[cfg(target_os = "windows")]
fn hardware_config_uses_device_context(
    config: &ffmpeg::ffi::AVCodecHWConfig,
    device_type: ffmpeg::ffi::AVHWDeviceType,
    pixel_format: ffmpeg::ffi::AVPixelFormat,
) -> bool {
    config.device_type == device_type
        && config.pix_fmt == pixel_format
        && (config.methods & ffmpeg::ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
}

#[cfg(target_os = "windows")]
fn open_hardware_device_decoder(
    stream: &ffmpeg::format::stream::Stream<'_>,
    codec: ffmpeg::Codec,
    device_type: ffmpeg::ffi::AVHWDeviceType,
    select_format: unsafe extern "C" fn(
        *mut ffmpeg::ffi::AVCodecContext,
        *const ffmpeg::ffi::AVPixelFormat,
    ) -> ffmpeg::ffi::AVPixelFormat,
) -> Result<ffmpeg::decoder::Video, String> {
    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|error| format!("could not create Windows hardware decoder: {error}"))?;
    let mut device = std::ptr::null_mut();
    let status = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device,
            device_type,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(format!(
            "could not create Windows hardware device: {}",
            ffmpeg::Error::from(status)
        ));
    }
    let context_device = unsafe { ffmpeg::ffi::av_buffer_ref(device) };
    if context_device.is_null() {
        unsafe { ffmpeg::ffi::av_buffer_unref(&mut device) };
        return Err("could not retain Windows hardware device for decoder".to_owned());
    }
    unsafe {
        // AVCodecContext owns this duplicate; release our creation reference after opening.
        (*context.as_mut_ptr()).hw_device_ctx = context_device;
        (*context.as_mut_ptr()).get_format = Some(select_format);
    }
    let result = context
        .decoder()
        .open_as(codec)
        .and_then(|opened| opened.video())
        .map_err(|error| format!("could not open Windows hardware decoder: {error}"));
    unsafe { ffmpeg::ffi::av_buffer_unref(&mut device) };
    result
}

#[cfg(target_os = "windows")]
unsafe extern "C" fn select_d3d11va_format(
    context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    unsafe {
        select_hardware_format(
            context,
            formats,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
        )
    }
}

#[cfg(target_os = "windows")]
unsafe extern "C" fn select_dxva2_format(
    context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    unsafe {
        select_hardware_format(
            context,
            formats,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_DXVA2_VLD,
        )
    }
}

#[cfg(target_os = "windows")]
unsafe fn select_hardware_format(
    _context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
    desired: ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    let mut format = formats;
    while unsafe { *format } != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if unsafe { *format } == desired {
            return desired;
        }
        format = unsafe { format.add(1) };
    }
    ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

#[cfg(target_os = "macos")]
fn open_videotoolbox_decoder(
    stream: &ffmpeg::format::stream::Stream<'_>,
) -> Result<ffmpeg::decoder::Video, String> {
    let codec_id = stream.parameters().id();
    let codec = ffmpeg::decoder::find(codec_id)
        .ok_or_else(|| format!("could not find video decoder for {codec_id:?}"))?;
    if !codec_supports_videotoolbox(&codec) {
        return Err("video decoder has no VideoToolbox hardware configuration".to_owned());
    }

    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|error| format!("could not create VideoToolbox decoder: {error}"))?;
    let mut device = std::ptr::null_mut();
    let status = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            &mut device,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(format!(
            "could not create VideoToolbox device: {}",
            ffmpeg::Error::from(status)
        ));
    }
    let context_device = unsafe { ffmpeg::ffi::av_buffer_ref(device) };
    if context_device.is_null() {
        unsafe { ffmpeg::ffi::av_buffer_unref(&mut device) };
        return Err("could not retain VideoToolbox device for decoder".to_owned());
    }
    unsafe {
        // AVCodecContext owns this duplicate; release our creation reference after opening.
        (*context.as_mut_ptr()).hw_device_ctx = context_device;
        (*context.as_mut_ptr()).get_format = Some(select_videotoolbox_format);
    }
    let result = context
        .decoder()
        .open_as(codec)
        .and_then(|opened| opened.video())
        .map_err(|error| format!("could not open VideoToolbox decoder: {error}"));
    unsafe { ffmpeg::ffi::av_buffer_unref(&mut device) };
    result
}

#[cfg(target_os = "macos")]
fn codec_supports_videotoolbox(codec: &ffmpeg::Codec) -> bool {
    let mut index = 0;
    loop {
        let config = unsafe { ffmpeg::ffi::avcodec_get_hw_config(codec.as_ptr(), index) };
        if config.is_null() {
            return false;
        }
        let config = unsafe { &*config };
        if config.device_type == ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX
            && config.pix_fmt == ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX
            && (config.methods & ffmpeg::ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
        {
            return true;
        }
        index += 1;
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn select_videotoolbox_format(
    _context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    let mut format = formats;
    while unsafe { *format } != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if unsafe { *format } == ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX {
            return ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX;
        }
        format = unsafe { format.add(1) };
    }
    ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

fn scale_monitor_frame(
    scaler: &mut Option<ScalingContext>,
    scaler_input: &mut Option<(Pixel, u32, u32)>,
    scaler_high_quality: &mut Option<bool>,
    scaled_size: &mut (u32, u32),
    decoded: &Video,
    transfer_hardware_frame: bool,
    output_size: (u32, u32),
    high_quality_scaling: bool,
    stage_timings: &DecoderStageTimingAccumulators,
) -> Result<Video, String> {
    let mut rgba_frame = Video::empty();
    if transfer_hardware_frame {
        let mut software_frame = Video::empty();
        let status = {
            let _timer = StageTimer::new(&stage_timings.hardware_transfer);
            unsafe {
                ffmpeg::ffi::av_hwframe_transfer_data(
                    software_frame.as_mut_ptr(),
                    decoded.as_ptr(),
                    0,
                )
            }
        };
        if status < 0 {
            return Err(format!(
                "could not transfer hardware frame to CPU: {}",
                ffmpeg::Error::from(status)
            ));
        }
        let input = (
            software_frame.format(),
            software_frame.width(),
            software_frame.height(),
        );
        let required_scaled_size = fitted_size(input.1, input.2, output_size.0, output_size.1);
        if *scaler_input != Some(input)
            || *scaled_size != required_scaled_size
            || *scaler_high_quality != Some(high_quality_scaling)
        {
            *scaler = Some(
                ScalingContext::get(
                    input.0,
                    input.1,
                    input.2,
                    Pixel::RGBA,
                    required_scaled_size.0,
                    required_scaled_size.1,
                    scaling_flags(high_quality_scaling),
                )
                .map_err(|error| format!("could not create RGBA scaler: {error}"))?,
            );
            *scaler_input = Some(input);
            *scaled_size = required_scaled_size;
            *scaler_high_quality = Some(high_quality_scaling);
        }
        {
            let _timer = StageTimer::new(&stage_timings.scaler);
            scaler
                .as_mut()
                .expect("hardware scaler initialized from transferred frame")
                .run(&software_frame, &mut rgba_frame)
                .map_err(|error| format!("could not scale monitor frame: {error}"))?;
        }
    } else {
        {
            let _timer = StageTimer::new(&stage_timings.scaler);
            scaler
                .as_mut()
                .expect("software scaler initialized when monitor opens")
                .run(decoded, &mut rgba_frame)
                .map_err(|error| format!("could not scale monitor frame: {error}"))?;
        }
    }
    Ok(rgba_frame)
}

const fn scaling_flags(high_quality_scaling: bool) -> ScalingFlags {
    if high_quality_scaling {
        ScalingFlags::BICUBIC
    } else {
        ScalingFlags::BILINEAR
    }
}

const fn requires_cpu_frame_transfer(backend: DecodeBackend) -> bool {
    matches!(
        backend,
        DecodeBackend::VideoToolbox | DecodeBackend::D3D11VA | DecodeBackend::DXVA2
    )
}

fn hardware_decoder_candidates(codec: Id) -> &'static [(&'static str, DecodeBackend)] {
    match codec {
        // Named decoders are fallbacks when the platform-native hardware context is unavailable.
        // CUVID precedes Quick Sync so a discrete NVIDIA adapter remains the first such fallback.
        Id::H264 => &[
            ("h264_cuvid", DecodeBackend::Nvidia),
            ("h264_qsv", DecodeBackend::IntelQuickSync),
        ],
        Id::HEVC => &[
            ("hevc_cuvid", DecodeBackend::Nvidia),
            ("hevc_qsv", DecodeBackend::IntelQuickSync),
        ],
        Id::AV1 => &[
            ("av1_cuvid", DecodeBackend::Nvidia),
            ("av1_qsv", DecodeBackend::IntelQuickSync),
        ],
        _ => &[],
    }
}

fn fitted_size(source_width: u32, source_height: u32, width: u32, height: u32) -> (u32, u32) {
    let scale = (width as f64 / source_width.max(1) as f64)
        .min(height as f64 / source_height.max(1) as f64);
    (
        (source_width.max(1) as f64 * scale).round().max(1.0) as u32,
        (source_height.max(1) as f64 * scale).round().max(1.0) as u32,
    )
}

fn copy_rgba_frame(frame: &Video, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let row_bytes = width as usize * 4;
    let stride = frame.stride(0);
    if stride < row_bytes || frame.data(0).len() < stride * height as usize {
        return Err("monitor scaler returned a truncated RGBA frame".to_owned());
    }
    let data = frame.data(0);
    let mut rgba = vec![0; row_bytes * height as usize];
    for row in 0..height as usize {
        rgba[row * row_bytes..(row + 1) * row_bytes]
            .copy_from_slice(&data[row * stride..row * stride + row_bytes]);
    }
    Ok(rgba)
}

fn letterbox_rgba(scaled: Vec<u8>, scaled_size: (u32, u32), output_size: (u32, u32)) -> Vec<u8> {
    if scaled_size == output_size {
        return scaled;
    }
    let (scaled_width, scaled_height) = scaled_size;
    let (width, height) = output_size;
    let mut output = vec![0; width as usize * height as usize * 4];
    let x_offset = (width - scaled_width) as usize / 2;
    let y_offset = (height - scaled_height) as usize / 2;
    let source_row = scaled_width as usize * 4;
    let output_row = width as usize * 4;
    for row in 0..scaled_height as usize {
        let start = (row + y_offset) * output_row + x_offset * 4;
        output[start..start + source_row]
            .copy_from_slice(&scaled[row * source_row..(row + 1) * source_row]);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        process::{Command, Stdio},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static UNIQUE: AtomicU64 = AtomicU64::new(0);
    static HARDWARE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn scrub_progress_pacer_allows_the_first_frame() {
        let now = Instant::now();
        assert!(scrub_progress_due(None, now, SCRUB_PROGRESS_INTERVAL));
    }

    #[test]
    fn scrub_progress_pacer_suppresses_a_burst_regardless_of_source_timestamp() {
        let now = Instant::now();
        for source_tick in [0, 41_667, 5_000_000, 90_000_000] {
            assert!(scrub_progress_moves_toward_target(None, 0, source_tick));
            assert!(!scrub_progress_due(Some(now), now, SCRUB_PROGRESS_INTERVAL));
        }
    }

    #[test]
    fn scrub_progress_pacer_allows_a_frame_at_the_interval() {
        let now = Instant::now();
        assert!(scrub_progress_due(
            Some(now),
            now + SCRUB_PROGRESS_INTERVAL,
            SCRUB_PROGRESS_INTERVAL
        ));
    }

    #[test]
    fn scrub_progress_pacer_does_not_decimate_media_frame_rates() {
        let now = Instant::now();
        for source_ticks in [
            &[0, 41_667, 83_333][..],                  // 24 fps
            &[0, 33_333, 66_667, 100_000][..],         // 30 fps
            &[0, 16_667, 33_333, 50_000, 66_667][..],  // 60 fps
            &[0, 10_000, 56_000, 58_000, 121_000][..], // VFR
        ] {
            let mut last_source_tick = None;
            let mut last_published = None;
            for (index, &source_tick) in source_ticks.iter().enumerate() {
                assert!(last_source_tick.is_none_or(|last| source_tick >= last));
                let published_at = now + SCRUB_PROGRESS_INTERVAL * index as u32;
                assert!(scrub_progress_due(
                    last_published,
                    published_at,
                    SCRUB_PROGRESS_INTERVAL
                ));
                last_source_tick = Some(source_tick);
                last_published = Some(published_at);
            }
        }
    }

    #[test]
    fn forward_scrub_progress_never_replays_an_older_keyframe() {
        assert!(!scrub_progress_moves_toward_target(
            Some(5_000_000),
            6_000_000,
            400_000
        ));
        assert!(scrub_progress_moves_toward_target(
            Some(5_000_000),
            6_000_000,
            5_400_000
        ));
    }

    #[test]
    fn reverse_or_random_scrub_progress_hides_below_target_preroll() {
        assert!(!scrub_progress_moves_toward_target(
            Some(6_000_000),
            5_000_000,
            400_000
        ));
        assert!(scrub_progress_moves_toward_target(
            Some(6_000_000),
            5_000_000,
            5_400_000
        ));
        assert!(!scrub_progress_moves_toward_target(
            None, 5_000_000, 400_000
        ));
    }

    #[test]
    fn scrub_progress_direction_remains_independent_of_wall_clock_pacing() {
        let now = Instant::now();
        assert!(!scrub_progress_moves_toward_target(
            Some(100_000),
            200_000,
            99_999
        ));
        assert!(scrub_progress_due(
            Some(now),
            now + SCRUB_PROGRESS_INTERVAL,
            SCRUB_PROGRESS_INTERVAL
        ));
    }

    fn hardware_test_guard() -> std::sync::MutexGuard<'static, ()> {
        HARDWARE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn tiny_media() -> PathBuf {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("maelstrom-monitor-{nanos}-{sequence}.mp4"));
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=64x48:r=24:d=1",
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
            ])
            .arg(&path)
            .status()
            .expect("start FFmpeg test fixture");
        assert!(status.success());
        path
    }

    fn tiny_still() -> PathBuf {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("maelstrom-monitor-{nanos}-{sequence}.bmp"));
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x0c2238:s=64x48:r=1",
                "-frames:v",
                "1",
                "-c:v",
                "bmp",
            ])
            .arg(&path)
            .status()
            .expect("start FFmpeg still fixture");
        assert!(status.success());
        path
    }

    fn request(path: PathBuf, request_id: u64) -> DecodeRequest {
        DecodeRequest {
            project_epoch: 8,
            cache_epoch: 3,
            request_id,
            media_id: 3,
            path,
            source_tick: 0,
            width: 40,
            height: 30,
            is_scrubbing: false,
            prewarm_scrub_workers: false,
            high_quality_scaling: true,
            progressive_scrub_frames: false,
            source_frame_duration_tick: None,
            acceleration: AccelerationPreference::Software,
        }
    }

    fn receive_for(decoder: &MonitorDecoder, request: &DecodeRequest) -> DecodeEvent {
        receive_matching(decoder, |frame| frame.source_tick >= request.source_tick)
    }

    fn receive_matching(
        decoder: &MonitorDecoder,
        accept: impl Fn(&DecodedFrame) -> bool,
    ) -> DecodeEvent {
        for _ in 0..500 {
            if let Some(event) = decoder.try_recv().unwrap() {
                match &event {
                    DecodeEvent::Error(_) => return event,
                    DecodeEvent::Frame(frame) if accept(frame) => return event,
                    DecodeEvent::Frame(_) => {}
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("decoder did not deliver an event")
    }

    fn assert_frame_reaches_target(frame: &DecodedFrame, request: &DecodeRequest) {
        assert_eq!(frame.request_id, request.request_id);
        assert_eq!(frame.media_id, request.media_id);
        assert!(
            frame.source_tick >= request.source_tick,
            "decoder published preroll frame {} before request target {}",
            frame.source_tick,
            request.source_tick,
        );
        assert!(
            frame.backend.is_some(),
            "freshly decoded frame must report its actual backend"
        );
    }

    #[test]
    fn dimensions_are_clamped_and_allocation_is_bounded() {
        let (width, height, bytes) = bounded_dimensions(u32::MAX, u32::MAX);
        assert!((1..=MAX_DIMENSION).contains(&width));
        assert!((1..=MAX_DIMENSION).contains(&height));
        assert!(bytes <= MAX_FRAME_BYTES);
        assert_eq!(bounded_dimensions(0, 0), (1, 1, 4));
    }

    #[test]
    fn decoder_stage_timing_snapshot_merges_with_saturation_and_zero_means() {
        let first = AtomicStageTiming::default();
        first.record(Duration::from_nanos(2_000_000));
        first.record(Duration::from_nanos(3_000_000));
        let first_snapshot = first.snapshot();
        assert_eq!(first_snapshot.samples, 2);
        assert_eq!(first_snapshot.total_nanos, 5_000_000);
        assert_eq!(first_snapshot.max_nanos, 3_000_000);
        assert!(first_snapshot.total_nanos >= first_snapshot.max_nanos);
        let mut merged = MonitorStageTiming::default();
        merged.merge(first_snapshot);
        merged.merge(MonitorStageTiming {
            samples: 1,
            total_nanos: 5_000_000,
            max_nanos: 5_000_000,
        });
        assert_eq!(merged.samples, 3);
        assert_eq!(merged.total_nanos, 10_000_000);
        assert_eq!(merged.max_nanos, 5_000_000);
        assert!((merged.total_ms() - 10.0).abs() < f64::EPSILON);
        assert!((merged.mean_ms() - (10.0 / 3.0)).abs() < f64::EPSILON);
        assert_eq!(MonitorStageTiming::default().mean_ms(), 0.0);

        let mut saturated = MonitorStageTiming {
            samples: u64::MAX,
            total_nanos: u64::MAX,
            max_nanos: 1,
        };
        saturated.merge(MonitorStageTiming {
            samples: 1,
            total_nanos: 1,
            max_nanos: 2,
        });
        assert_eq!(saturated.samples, u64::MAX);
        assert_eq!(saturated.total_nanos, u64::MAX);
        assert_eq!(saturated.max_nanos, 2);
    }

    #[test]
    fn decoder_stage_timing_snapshot_remains_coherent_while_single_lane_writes() {
        let timing = Arc::new(AtomicStageTiming::default());
        let writer_timing = Arc::clone(&timing);
        let writer = thread::spawn(move || {
            for _ in 0..20_000 {
                writer_timing.record(Duration::from_nanos(7));
            }
        });
        while !writer.is_finished() {
            let snapshot = timing.snapshot();
            assert_eq!(snapshot.samples == 0, snapshot.total_nanos == 0);
            assert_eq!(snapshot.samples == 0, snapshot.max_nanos == 0);
            assert!(snapshot.total_nanos >= snapshot.max_nanos);
        }
        writer.join().expect("timing writer");
        let snapshot = timing.snapshot();
        assert_eq!(snapshot.samples, 20_000);
        assert_eq!(snapshot.total_nanos, 140_000);
        assert_eq!(snapshot.max_nanos, 7);
    }

    #[test]
    fn supplied_media_software_decode_reports_applicable_stage_timings() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let decoder = MonitorDecoder::new();
        let desired = request(PathBuf::from(path), 740);
        decoder.request(desired.clone()).unwrap();
        match receive_for(&decoder, &desired) {
            DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &desired),
            DecodeEvent::Error(error) => panic!("software timing decode failed: {}", error.message),
        }
        let timings = decoder.stage_timings();
        for stage in [
            timings.cache_lookup,
            timings.demux_packet,
            timings.decoder_calls,
            timings.scaler,
            timings.rgba_copy_letterbox,
            timings.worker_request,
        ] {
            assert!(stage.samples >= 1);
            assert!(stage.total_nanos >= stage.max_nanos);
        }
        assert_eq!(timings.hardware_transfer.samples, 0);
    }

    #[test]
    fn backend_identity_and_cpu_transfer_requirements_are_stable() {
        assert_eq!(
            DecodeBackend::VideoToolbox.display_name(),
            "Apple VideoToolbox"
        );
        assert_eq!(DecodeBackend::D3D11VA.display_name(), "Windows D3D11VA");
        assert_eq!(DecodeBackend::DXVA2.display_name(), "Windows DXVA2");
        assert!(requires_cpu_frame_transfer(DecodeBackend::VideoToolbox));
        assert!(requires_cpu_frame_transfer(DecodeBackend::D3D11VA));
        assert!(requires_cpu_frame_transfer(DecodeBackend::DXVA2));
        assert!(!requires_cpu_frame_transfer(DecodeBackend::Software));
        assert!(!requires_cpu_frame_transfer(DecodeBackend::IntelQuickSync));
        assert!(!requires_cpu_frame_transfer(DecodeBackend::Nvidia));
    }

    #[test]
    fn hardware_runtime_failure_reopens_the_same_request_in_software() {
        let mut desired = request(PathBuf::from("runtime-fallback.mp4"), 77);
        desired.project_epoch = 9;
        desired.cache_epoch = 11;
        desired.media_id = 42;
        desired.source_tick = 3_250_000;
        desired.acceleration = AccelerationPreference::PreferHardware;

        for backend in [
            DecodeBackend::VideoToolbox,
            DecodeBackend::D3D11VA,
            DecodeBackend::DXVA2,
            DecodeBackend::Nvidia,
            DecodeBackend::IntelQuickSync,
        ] {
            let fallback = software_fallback_request(&desired, backend)
                .expect("hardware runtime errors require a software retry");
            assert_eq!(fallback.acceleration, AccelerationPreference::Software);
            let mut expected = desired.clone();
            expected.acceleration = AccelerationPreference::Software;
            assert_eq!(fallback, expected);
        }
        assert!(software_fallback_request(&desired, DecodeBackend::Software).is_none());

        desired.acceleration = AccelerationPreference::Software;
        assert!(software_fallback_request(&desired, DecodeBackend::D3D11VA).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_hardware_configuration_prefers_d3d11va_then_dxva2() {
        let candidates = windows_hardware_decoder_candidates();
        assert_eq!(candidates[0].backend, DecodeBackend::D3D11VA);
        assert_eq!(
            candidates[0].device_type,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA
        );
        assert_eq!(
            candidates[0].pixel_format,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11
        );
        assert_eq!(candidates[1].backend, DecodeBackend::DXVA2);
        assert_eq!(
            candidates[1].device_type,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2
        );
        assert_eq!(
            candidates[1].pixel_format,
            ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_DXVA2_VLD
        );

        let mut configuration = ffmpeg::ffi::AVCodecHWConfig {
            pix_fmt: candidates[0].pixel_format,
            methods: ffmpeg::ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32,
            device_type: candidates[0].device_type,
        };
        assert!(hardware_config_uses_device_context(
            &configuration,
            candidates[0].device_type,
            candidates[0].pixel_format,
        ));
        configuration.methods = 0;
        assert!(!hardware_config_uses_device_context(
            &configuration,
            candidates[0].device_type,
            candidates[0].pixel_format,
        ));
    }

    #[cfg(target_os = "windows")]
    fn open_supplied_media_windows_hardware_monitor(
        path: PathBuf,
        requested_backend: DecodeBackend,
    ) -> Result<StickyMonitor, String> {
        let input = ffmpeg::format::input(&path)
            .map_err(|error| format!("could not open supplied test media: {error}"))?;
        let (stream_index, time_base, decoder) = {
            let stream = input
                .streams()
                .best(Type::Video)
                .ok_or_else(|| "supplied test media has no video stream".to_owned())?;
            let candidate = windows_hardware_decoder_candidates()
                .into_iter()
                .find(|candidate| candidate.backend == requested_backend)
                .expect("requested Windows hardware backend is configured for this test");
            let codec = ffmpeg::decoder::find(stream.parameters().id())
                .ok_or_else(|| "could not find supplied-media video decoder".to_owned())?;
            if !codec_supports_hardware_config(
                &codec,
                candidate.device_type,
                candidate.pixel_format,
            ) {
                return Err(format!(
                    "{} is not advertised with HW_DEVICE_CTX for supplied media",
                    requested_backend.display_name()
                ));
            }
            let decoder = open_hardware_device_decoder(
                &stream,
                codec,
                candidate.device_type,
                candidate.select_format,
            )?;
            (stream.index(), stream.time_base(), decoder)
        };
        let output_size = (40, 30);
        let scaled_size = fitted_size(
            decoder.width(),
            decoder.height(),
            output_size.0,
            output_size.1,
        );
        Ok(StickyMonitor {
            path,
            input,
            stream_index,
            time_base,
            decoder,
            scaler: None,
            scaler_input: None,
            scaler_high_quality: None,
            output_size,
            scaled_size,
            last_source_tick: None,
            last_visible_tick: None,
            backend: requested_backend,
            transfer_hardware_frames: true,
        })
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn supplied_media_windows_hardware_backends_transfer_opaque_frames_to_cpu() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        for backend in [DecodeBackend::D3D11VA, DecodeBackend::DXVA2] {
            let mut monitor =
                open_supplied_media_windows_hardware_monitor(PathBuf::from(&path), backend)
                    .unwrap_or_else(|error| {
                        panic!("could not open {}: {error}", backend.display_name())
                    });
            assert_eq!(monitor.backend, backend);
            assert!(monitor.transfer_hardware_frames);

            let desired = request(PathBuf::from(&path), 920);
            let stage_timings = DecoderStageTimingAccumulators::default();
            let frame = monitor
                .decode(
                    &desired,
                    || false,
                    || None,
                    &mut |_| {},
                    &mut |_| {},
                    &stage_timings,
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "{} did not transfer and scale a frame: {error}",
                        backend.display_name()
                    )
                })
                .unwrap_or_else(|| panic!("{} frame decode was cancelled", backend.display_name()));
            assert_eq!((frame.width, frame.height), (40, 30));
            assert_eq!(frame.rgba.len(), 40 * 30 * 4);
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn supplied_media_d3d11va_runtime_failure_retains_software_and_latest_generation() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let path = PathBuf::from(path);
        let mut original = request(path.clone(), 930);
        original.acceleration = AccelerationPreference::PreferHardware;
        original.source_tick = 0;
        let mut newest = original.clone();
        newest.request_id = 931;
        newest.source_tick = 1_000_000;
        assert!(same_decode_generation(&original, &newest));

        let hardware = open_supplied_media_windows_hardware_monitor(path, DecodeBackend::D3D11VA)
            .expect("open D3D11VA monitor for forced runtime fallback");
        assert_eq!(hardware.backend, DecodeBackend::D3D11VA);
        let mut sessions = HashMap::from([(original.media_id, hardware)]);
        let commands = Arc::new(Mutex::new(Some(MonitorCommand::Request(newest.clone()))));
        let stage_timings = DecoderStageTimingAccumulators::default();

        let frame = recover_hardware_decode_failure(
            &mut sessions,
            &original,
            &commands,
            "injected D3D11VA runtime failure".to_owned(),
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &stage_timings,
        )
        .expect("software fallback decodes supplied media")
        .expect("same-generation replacement was not cancelled");

        assert_eq!(frame.request_id, newest.request_id);
        assert_eq!(frame.target_tick, newest.source_tick);
        assert!(frame.source_tick >= newest.source_tick);
        assert_eq!((frame.width, frame.height), (40, 30));
        assert_eq!(frame.rgba.len(), 40 * 30 * 4);
        assert_eq!(
            sessions
                .get(&original.media_id)
                .expect("software session retained after hardware failure")
                .backend,
            DecodeBackend::Software
        );
        let queued = commands.lock().unwrap();
        let Some(MonitorCommand::Request(queued)) = queued.as_ref() else {
            panic!("same-generation request was unexpectedly removed")
        };
        assert_eq!(queued.acceleration, AccelerationPreference::PreferHardware);
        assert!(same_decode_generation(&original, queued));
    }

    #[test]
    fn exact_cached_target_returns_without_opening_media() {
        let desired = request(PathBuf::from("this-file-must-not-be-opened.mp4"), 91);
        let cache = Arc::new(Mutex::new(MonitorFrameCache::new(8 * 1024)));
        cache.lock().unwrap().prepare_request(&desired);
        let rgba: Arc<[u8]> = vec![17; 40 * 30 * 4].into();
        assert!(cache.lock().unwrap().insert(
            frame_cache_key(&desired, desired.source_tick),
            FrameValue::new(desired.source_tick, 40, 30, Arc::clone(&rgba)),
        ));
        let mut sessions = HashMap::new();
        let commands = Arc::new(Mutex::new(None));
        let stage_timings = DecoderStageTimingAccumulators::default();
        let mut session_states = Vec::new();

        let event = decode_monitor_request(
            &mut sessions,
            &cache,
            &desired,
            &commands,
            &mut |_| {},
            &mut |active| session_states.push(active),
            &stage_timings,
        )
        .expect("cache hit returns a frame");
        let DecodeEvent::Frame(frame) = event else {
            panic!("cache hit returned an error")
        };
        assert_eq!(frame.request_id, 91);
        assert_eq!(frame.rgba, rgba);
        assert!(sessions.is_empty());
        assert_eq!(session_states, vec![false]);
    }

    #[test]
    fn scaler_policy_change_invalidates_cached_monitor_pixels() {
        let original = request(PathBuf::from("scaler-policy.mp4"), 1);
        let mut cache = MonitorFrameCache::new(8 * 1024);
        cache.prepare_request(&original);
        let key = frame_cache_key(&original, 0);
        assert!(cache.insert_if_current(
            &original,
            key,
            FrameValue::new(0, 40, 30, vec![0; 40 * 30 * 4].into()),
        ));
        assert!(cache.get(&key).is_some());

        let mut changed = original.clone();
        changed.high_quality_scaling = false;
        assert!(cache.prepare_request(&changed));
        assert!(cache.get(&key).is_none());
        assert!(!cache.insert_if_current(
            &original,
            key,
            FrameValue::new(0, 40, 30, vec![1; 40 * 30 * 4].into()),
        ));
        assert!(cache.get(&key).is_none());
        assert!(cache.insert_if_current(
            &changed,
            key,
            FrameValue::new(0, 40, 30, vec![2; 40 * 30 * 4].into()),
        ));
    }

    #[test]
    fn scaler_policy_selects_requested_ffmpeg_filter() {
        assert_eq!(scaling_flags(true), ScalingFlags::BICUBIC);
        assert_eq!(scaling_flags(false), ScalingFlags::BILINEAR);
    }

    #[test]
    fn active_monitor_session_retention_keeps_same_source_and_evicts_others() {
        let mut sessions = HashMap::from([(11_u32, "first"), (22_u32, "second")]);

        retain_active_monitor_session(&mut sessions, 22);
        assert_eq!(sessions, HashMap::from([(22_u32, "second")]));

        // Repeated requests for the active source preserve its sticky decoder context.
        retain_active_monitor_session(&mut sessions, 22);
        assert_eq!(sessions, HashMap::from([(22_u32, "second")]));

        // A different request cannot inherit or accumulate inactive sessions; per worker cap is 1.
        retain_active_monitor_session(&mut sessions, 33);
        assert!(sessions.is_empty());
        assert!(sessions.len() <= MONITOR_WORKER_COUNT);
    }

    #[test]
    fn frame_cache_diagnostics_track_current_peak_and_clear() {
        let diagnostics = Arc::new(DecoderResourceDiagnostics::new(32));
        let mut cache = MonitorFrameCache::new_with_diagnostics(32, Arc::clone(&diagnostics));
        let key = FrameKey {
            project_epoch: 1,
            media_id: 2,
            source_tick: 0,
            width: 2,
            height: 2,
        };

        assert!(cache.insert(key, FrameValue::new(0, 2, 2, vec![0; 16].into())));
        assert!(cache.insert(
            FrameKey {
                source_tick: SPARSE_CACHE_INTERVAL_TICKS,
                ..key
            },
            FrameValue::new(SPARSE_CACHE_INTERVAL_TICKS, 2, 2, vec![1; 16].into()),
        ));
        assert!(cache.insert(
            FrameKey {
                source_tick: SPARSE_CACHE_INTERVAL_TICKS * 2,
                ..key
            },
            FrameValue::new(SPARSE_CACHE_INTERVAL_TICKS * 2, 2, 2, vec![2; 16].into()),
        ));
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.frame_cache_capacity_bytes, 32);
        assert_eq!(snapshot.current_frame_cache_bytes, 32);
        assert_eq!(snapshot.peak_frame_cache_bytes, 32);
        assert!(snapshot.current_frame_cache_bytes <= snapshot.frame_cache_capacity_bytes);

        cache.clear();
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.current_frame_cache_bytes, 0);
        assert_eq!(snapshot.peak_frame_cache_bytes, 32);
    }

    #[test]
    fn worker_session_diagnostics_aggregate_with_fixed_cap() {
        let diagnostics = DecoderResourceDiagnostics::new(0);
        diagnostics.publish_worker_session(0, true);
        diagnostics.publish_worker_session(2, true);
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.active_sticky_sessions, 2);
        assert_eq!(snapshot.peak_sticky_sessions, 2);
        assert_eq!(snapshot.session_cap, MONITOR_WORKER_COUNT);

        diagnostics.publish_worker_session(0, false);
        diagnostics.publish_worker_session(2, false);
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.active_sticky_sessions, 0);
        assert_eq!(snapshot.peak_sticky_sessions, 2);
        assert!(snapshot.peak_sticky_sessions <= snapshot.session_cap);
    }

    #[test]
    fn diagnostics_snapshots_remain_peak_coherent_while_publishing() {
        let diagnostics = Arc::new(DecoderResourceDiagnostics::new(128));
        let publishing = Arc::new(AtomicBool::new(true));
        let writer_diagnostics = Arc::clone(&diagnostics);
        let writer_publishing = Arc::clone(&publishing);
        let writer = thread::spawn(move || {
            for _ in 0..10_000 {
                writer_diagnostics.publish_cache_bytes(128);
                writer_diagnostics.publish_worker_session(0, true);
                writer_diagnostics.publish_cache_bytes(0);
                writer_diagnostics.publish_worker_session(0, false);
            }
            writer_publishing.store(false, Ordering::Release);
        });

        while publishing.load(Ordering::Acquire) {
            let snapshot = diagnostics.snapshot();
            assert!(snapshot.current_frame_cache_bytes <= snapshot.peak_frame_cache_bytes);
            assert!(snapshot.active_sticky_sessions <= snapshot.peak_sticky_sessions);
        }
        writer.join().expect("diagnostics publisher thread");
    }

    #[test]
    fn continuous_scrub_retains_sparse_anchors_plus_exact_latest() {
        let mut cache = MonitorFrameCache::new(1024 * 1024);
        for index in 0..20_i64 {
            let frame_key = FrameKey {
                project_epoch: 1,
                media_id: 2,
                source_tick: index * 50_000,
                width: 2,
                height: 2,
            };
            assert!(cache.insert(
                frame_key,
                FrameValue::new(frame_key.source_tick, 2, 2, vec![0; 16].into()),
            ));
        }

        assert_eq!(cache.frames.len(), 5);
        assert!(
            cache
                .get(&FrameKey {
                    project_epoch: 1,
                    media_id: 2,
                    source_tick: 950_000,
                    width: 2,
                    height: 2,
                })
                .is_some()
        );
    }

    #[test]
    fn scrub_cache_returns_only_nearby_frames_at_or_after_target() {
        let mut cache = MonitorFrameCache::new(1024 * 1024);
        let mut desired = request(PathBuf::from("scrub-source.mp4"), 1);
        desired.is_scrubbing = true;
        desired.source_tick = 950_000;
        cache.prepare_request(&desired);
        cache.insert_scrub_traversal(
            &desired,
            &DecodedRgba {
                request_id: desired.request_id,
                target_tick: 1_000_000,
                source_tick: 1_000_000,
                width: desired.width,
                height: desired.height,
                rgba: vec![0; 40 * 30 * 4].into(),
            },
        );

        assert_eq!(
            cache
                .get_scrub_at_or_after(&desired)
                .expect("nearby future traversal frame")
                .source_tick,
            1_000_000
        );
        desired.source_frame_duration_tick = Some(33_334);
        assert!(
            cache.get_scrub_at_or_after(&desired).is_none(),
            "known-rate cache lookup must not jump more than one source frame"
        );
        desired.source_tick = 966_666;
        assert_eq!(
            cache
                .get_scrub_at_or_after(&desired)
                .expect("next source frame remains eligible")
                .source_tick,
            1_000_000
        );
        desired.source_frame_duration_tick = Some(8_334);
        desired.source_tick = 991_666;
        assert_eq!(
            cache
                .get_scrub_at_or_after(&desired)
                .expect("fallback-rate cache lookup stays within one fallback frame")
                .source_tick,
            1_000_000
        );
        desired.source_tick = 991_665;
        assert!(cache.get_scrub_at_or_after(&desired).is_none());
        desired.source_tick = 1_000_001;
        assert!(cache.get_scrub_at_or_after(&desired).is_none());
    }

    #[test]
    fn project_or_source_change_releases_frames_and_sparse_bookkeeping() {
        let mut cache = MonitorFrameCache::new(1024 * 1024);
        let original = request(PathBuf::from("source-a.mp4"), 1);
        cache.prepare_request(&original);
        let original_key = frame_cache_key(&original, 0);
        assert!(cache.insert(
            original_key,
            FrameValue::new(0, 40, 30, vec![0; 40 * 30 * 4].into()),
        ));
        assert!(!cache.frames.is_empty());

        let mut relinked = original.clone();
        relinked.path = PathBuf::from("source-b.mp4");
        cache.prepare_request(&relinked);
        assert!(cache.frames.is_empty());
        assert!(cache.latest.is_empty());
        assert!(cache.last_anchor_bucket.is_empty());
        assert_eq!(cache.sources.get(&relinked.media_id), Some(&relinked.path));

        assert!(cache.insert(
            frame_cache_key(&relinked, 0),
            FrameValue::new(0, 40, 30, vec![0; 40 * 30 * 4].into()),
        ));
        let mut next_project = relinked.clone();
        next_project.cache_epoch += 1;
        cache.prepare_request(&next_project);
        assert!(cache.frames.is_empty());
        assert!(cache.latest.is_empty());
        assert_eq!(cache.project_epoch, Some(next_project.cache_epoch));
    }

    #[test]
    fn failed_or_uncached_sources_cannot_grow_stream_registry_without_bound() {
        let mut cache = MonitorFrameCache::new(0);
        for media_id in 0..=MAX_CACHE_STREAM_STATES as u32 {
            let mut desired = request(PathBuf::from(format!("missing-{media_id}.mp4")), 1);
            desired.media_id = media_id;
            let _ = cache.prepare_request(&desired);
            assert!(cache.sources.len() <= MAX_CACHE_STREAM_STATES);
        }
    }

    #[test]
    fn same_generation_replacements_preserve_valid_decode_progress() {
        let current = request(PathBuf::from("clip.mp4"), 1);
        let mut near_forward = current.clone();
        near_forward.source_tick = FORWARD_REUSE_TICKS;
        assert!(same_decode_generation(&current, &near_forward));

        let mut backward = current.clone();
        backward.source_tick = -1;
        assert!(same_decode_generation(&current, &backward));

        let mut distant = current.clone();
        distant.source_tick = FORWARD_REUSE_TICKS + 1;
        assert!(same_decode_generation(&current, &distant));

        let mut scrub = current.clone();
        scrub.is_scrubbing = true;
        assert!(
            !same_decode_generation(&current, &scrub),
            "scrub/release transitions must not coalesce under the old mode"
        );

        let mut lower_quality_scaler = current.clone();
        lower_quality_scaler.high_quality_scaling = false;
        assert!(
            !same_decode_generation(&current, &lower_quality_scaler),
            "a scaler-policy change must rebuild the sticky decoder generation"
        );

        let mut progressive_scrub = scrub.clone();
        progressive_scrub.progressive_scrub_frames = true;
        assert!(
            !same_decode_generation(&scrub, &progressive_scrub),
            "full-quality and progressive scrub policies cannot share traversal work"
        );

        let mut probed_rate = current.clone();
        probed_rate.source_frame_duration_tick = Some(33_334);
        assert!(!same_decode_generation(&current, &probed_rate));

        let mut other_media = near_forward;
        other_media.media_id += 1;
        assert!(!same_decode_generation(&current, &other_media));
    }

    #[test]
    fn newest_request_wins_without_stale_frame_when_ffmpeg_is_available() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let decoder = MonitorDecoder::new();
        decoder.request(request(path.clone(), 1)).unwrap();
        decoder.request(request(path.clone(), 2)).unwrap();
        match receive_matching(&decoder, |frame| frame.request_id == 2) {
            DecodeEvent::Frame(frame) => {
                assert_eq!(frame.request_id, 2);
                assert_eq!(frame.project_epoch, 8);
                assert_eq!(frame.rgba.len(), 40 * 30 * 4);
            }
            DecodeEvent::Error(error) => panic!("unexpected decode error: {}", error.message),
        }
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn monitor_decoder_produces_a_frame_for_a_still_image() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_still();
        let decoder = MonitorDecoder::new();
        let still_request = request(path.clone(), 1);
        decoder.request(still_request.clone()).unwrap();
        match receive_for(&decoder, &still_request) {
            DecodeEvent::Frame(frame) => {
                assert_eq!(frame.source_tick, 0);
                assert_eq!((frame.width, frame.height), (40, 30));
                assert_eq!(frame.rgba.len(), 40 * 30 * 4);
            }
            DecodeEvent::Error(error) => panic!("unexpected still decode error: {}", error.message),
        }
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sticky_monitor_reuses_its_open_decoder_for_near_forward_targets() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let first = request(path.clone(), 1);
        let mut monitor = StickyMonitor::open(&first).expect("open sticky monitor");
        let stage_timings = DecoderStageTimingAccumulators::default();
        let first_frame = monitor
            .decode(
                &first,
                || false,
                || None,
                &mut |_| {},
                &mut |_| {},
                &stage_timings,
            )
            .expect("decode first frame")
            .expect("first frame was not superseded");
        let mut second = first.clone();
        second.request_id = 2;
        second.source_tick = 250_000;
        let second_frame = monitor
            .decode(
                &second,
                || false,
                || None,
                &mut |_| {},
                &mut |_| {},
                &stage_timings,
            )
            .expect("decode forward frame")
            .expect("forward frame was not superseded");
        assert_eq!(first_frame.rgba.len(), 40 * 30 * 4);
        assert_eq!(second_frame.rgba.len(), 40 * 30 * 4);
        assert!(monitor.last_source_tick.is_some());
        drop(monitor);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancellation_and_drop_cleanly_stop_scheduler() {
        let decoder = MonitorDecoder::new();
        decoder.cancel_pending().unwrap();
        decoder.reset_live_cache().unwrap();
        drop(decoder);
    }

    #[test]
    fn completed_decode_notifies_owner_without_polling() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let decoder = MonitorDecoder::new_with_notifier(move || {
            let _ = tx.try_send(());
        });
        decoder.request(request(path.clone(), 77)).unwrap();

        rx.recv_timeout(Duration::from_secs(2))
            .expect("decoder completion did not wake its owner");
        assert!(matches!(
            decoder.try_recv().unwrap(),
            Some(DecodeEvent::Frame(frame)) if frame.request_id == 77
        ));

        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn continuous_same_generation_targets_never_publish_preroll_frames() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let mut sessions = HashMap::new();
        let frame_cache = Arc::new(Mutex::new(MonitorFrameCache::new(0)));
        let commands = Arc::new(Mutex::new(None));
        let stage_timings = DecoderStageTimingAccumulators::default();
        let mut current = request(path.clone(), 30);
        current.source_tick = 700_000;
        let mut progress = Vec::new();
        for step in 1..=3 {
            let mut newer = current.clone();
            newer.request_id += 1;
            newer.source_tick = 700_000 + step * 50_000;
            *commands.lock().unwrap() = Some(MonitorCommand::Request(newer.clone()));
            match decode_monitor_request(
                &mut sessions,
                &frame_cache,
                &current,
                &commands,
                &mut |_| {},
                &mut |_| {},
                &stage_timings,
            ) {
                Some(DecodeEvent::Frame(frame)) => {
                    assert_frame_reaches_target(&frame, &newer);
                    progress.push(frame.source_tick);
                }
                Some(DecodeEvent::Error(error)) => {
                    panic!("progress decode failed: {}", error.message)
                }
                None => panic!("same-generation target was treated as invalidation"),
            }
            current = newer;
        }
        assert_eq!(progress.len(), 3);
        assert!(progress.windows(2).all(|pair| pair[1] >= pair[0]));
        drop(sessions);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn backward_same_generation_target_keeps_accurate_progress_visible() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let mut sessions = HashMap::new();
        let frame_cache = Arc::new(Mutex::new(MonitorFrameCache::new(0)));
        let commands = Arc::new(Mutex::new(None));
        let stage_timings = DecoderStageTimingAccumulators::default();
        let mut current = request(path.clone(), 41);
        current.source_tick = 700_000;
        let mut backward = current.clone();
        backward.request_id = 42;
        backward.source_tick = 100_000;
        *commands.lock().unwrap() = Some(MonitorCommand::Request(backward.clone()));
        match decode_monitor_request(
            &mut sessions,
            &frame_cache,
            &current,
            &commands,
            &mut |_| {},
            &mut |_| {},
            &stage_timings,
        ) {
            Some(DecodeEvent::Frame(frame)) => assert_frame_reaches_target(&frame, &backward),
            Some(DecodeEvent::Error(error)) => panic!("reverse progress failed: {}", error.message),
            None => panic!("reverse scrub cancelled every progressive frame"),
        }
        drop(sessions);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn supplied_media_decodes_forward_backward_and_far_targets() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let path = PathBuf::from(path);
        let decoder = MonitorDecoder::new();
        for (index, source_tick) in [0, 1_000_000, 7_000_000, 3_000_000, 12_000_000]
            .into_iter()
            .enumerate()
        {
            let request_id = index as u64 + 9;
            let mut desired = request(path.clone(), request_id);
            desired.source_tick = source_tick;
            decoder.request(desired.clone()).unwrap();
            match receive_for(&decoder, &desired) {
                DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &desired),
                DecodeEvent::Error(error) => panic!("supplied media failed: {}", error.message),
            }
        }
    }

    #[test]
    fn supplied_media_hardware_backward_seek_is_accurate_and_responsive() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let mut first = request(PathBuf::from(path), 501);
        first.acceleration = AccelerationPreference::PreferHardware;
        first.source_tick = 12_000_000;
        let mut monitor = StickyMonitor::open(&first).expect("open hardware monitor");
        let stage_timings = DecoderStageTimingAccumulators::default();
        #[cfg(target_os = "macos")]
        assert_eq!(monitor.backend, DecodeBackend::VideoToolbox);
        assert_ne!(
            monitor.backend,
            DecodeBackend::Software,
            "this GPU-equipped test machine unexpectedly fell back to software"
        );
        monitor
            .decode(
                &first,
                || false,
                || None,
                &mut |_| {},
                &mut |_| {},
                &stage_timings,
            )
            .expect("decode forward hardware target")
            .expect("forward hardware target was cancelled");

        let mut backward = first.clone();
        backward.request_id += 1;
        backward.source_tick = 3_000_000;
        backward.width = 64;
        backward.height = 36;
        let started = Instant::now();
        let frame = monitor
            .decode(
                &backward,
                || false,
                || None,
                &mut |_| {},
                &mut |_| {},
                &stage_timings,
            )
            .expect("decode backward hardware target")
            .expect("backward hardware target was cancelled");
        assert!(frame.source_tick >= backward.source_tick);
        assert_eq!((frame.width, frame.height), (64, 36));
        let backward_elapsed = started.elapsed();
        eprintln!("hardware 12s -> 3s backward seek: {backward_elapsed:?}");
        let limit = if cfg!(debug_assertions) {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(750)
        };
        assert!(
            backward_elapsed < limit,
            "hardware backward seek took {:?}",
            backward_elapsed
        );
    }

    #[test]
    fn supplied_media_repeated_reverse_scrub_stays_bounded() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let decoder = MonitorDecoder::new();
        for (index, source_tick) in [12_000_000, 3_000_000, 11_000_000, 2_000_000, 10_000_000]
            .into_iter()
            .enumerate()
        {
            let mut desired = request(PathBuf::from(&path), 600 + index as u64);
            desired.acceleration = AccelerationPreference::PreferHardware;
            desired.width = 640;
            desired.height = 360;
            desired.source_tick = source_tick;
            let started = Instant::now();
            decoder.request(desired.clone()).unwrap();
            match receive_for(&decoder, &desired) {
                DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &desired),
                DecodeEvent::Error(error) => panic!("reverse scrub failed: {}", error.message),
            }
            let elapsed = started.elapsed();
            eprintln!("reverse scrub target {source_tick}: {elapsed:?}");
            let limit = if cfg!(debug_assertions) {
                Duration::from_secs(3)
            } else {
                Duration::from_secs(1)
            };
            assert!(
                elapsed < limit,
                "reverse scrub target {source_tick} took {elapsed:?}"
            );
        }
    }

    #[test]
    fn supplied_media_full_quality_forward_storm_stays_current_during_drag() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let path = PathBuf::from(path);
        let decoder = MonitorDecoder::new();
        let mut paused = request(path.clone(), 99);
        paused.acceleration = AccelerationPreference::PreferHardware;
        paused.source_tick = 200_000;
        paused.width = 640;
        paused.height = 360;
        paused.prewarm_scrub_workers = true;
        decoder.request(paused.clone()).unwrap();
        match receive_for(&decoder, &paused) {
            DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &paused),
            DecodeEvent::Error(error) => panic!("paused prewarm failed: {}", error.message),
        }
        thread::sleep(Duration::from_millis(100));
        while decoder.try_recv().unwrap().is_some() {}
        let started = Instant::now();
        let mut published_while_moving = 0;
        let mut newest_published_request = 0;
        let mut requested_targets = HashMap::new();
        let mut progress = Vec::new();
        for step in 0..40_u64 {
            let request_id = 100 + step;
            let mut desired = request(path.clone(), request_id);
            desired.acceleration = AccelerationPreference::PreferHardware;
            desired.width = 640;
            desired.height = 360;
            desired.source_tick = 200_000 + step as i64 * 30_000;
            desired.is_scrubbing = true;
            desired.progressive_scrub_frames = true;
            desired.source_frame_duration_tick = Some(33_334);
            requested_targets.insert(request_id, desired.source_tick);
            decoder.request(desired).unwrap();
            if let Some(DecodeEvent::Frame(frame)) = decoder.try_recv().unwrap() {
                assert_eq!((frame.width, frame.height), (640, 360));
                assert!(
                    requested_targets.contains_key(&frame.request_id),
                    "forward scrub published an unknown request {}",
                    frame.request_id
                );
                progress.push(frame.source_tick);
                published_while_moving += 1;
                newest_published_request = newest_published_request.max(frame.request_id);
            }
            thread::sleep(Duration::from_millis(10));
        }
        eprintln!(
            "forward scrub published {published_while_moving} frames during 40 pointer updates"
        );
        assert!(
            published_while_moving >= 10,
            "forward scrub published only {published_while_moving} frames during 40 pointer updates"
        );
        assert!(
            newest_published_request >= 125,
            "forward scrub lagged more than fifteen pointer updates behind; newest was {newest_published_request}"
        );
        assert!(
            progress.windows(2).all(|ticks| ticks[1] >= ticks[0]),
            "forward scrub replayed earlier source frames: {progress:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn supplied_media_full_quality_backward_storm_stays_live_during_drag() {
        let Some(path) = std::env::var_os("MAELSTROM_TEST_MEDIA") else {
            return;
        };
        let _hardware = hardware_test_guard();
        let path = PathBuf::from(path);
        let decoder = MonitorDecoder::new();
        let mut initial = request(path.clone(), 800);
        initial.acceleration = AccelerationPreference::PreferHardware;
        initial.source_tick = 12_000_000;
        initial.prewarm_scrub_workers = true;
        initial.width = 640;
        initial.height = 360;
        initial.source_frame_duration_tick = Some(33_334);
        decoder.request(initial.clone()).unwrap();
        match receive_for(&decoder, &initial) {
            DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &initial),
            DecodeEvent::Error(error) => panic!("initial hardware frame failed: {}", error.message),
        }
        thread::sleep(Duration::from_millis(100));
        while decoder.try_recv().unwrap().is_some() {}

        let mut requested_targets = HashMap::new();
        let mut published_while_moving = 0;
        let mut newest_published_request = 0;
        for step in 0..100_u64 {
            let request_id = 801 + step;
            let mut desired = request(path.clone(), request_id);
            desired.acceleration = AccelerationPreference::PreferHardware;
            desired.source_tick = 11_900_000 - step as i64 * 90_000;
            desired.is_scrubbing = true;
            desired.width = 640;
            desired.height = 360;
            desired.progressive_scrub_frames = true;
            desired.source_frame_duration_tick = Some(33_334);
            requested_targets.insert(request_id, desired.source_tick);
            decoder.request(desired).unwrap();
            // The real application polls from the decoder notifier on the next UI turn. Give the
            // worker the same pointer-sample interval before observing its one-slot event.
            thread::sleep(Duration::from_millis(10));
            if let Some(DecodeEvent::Frame(frame)) = decoder.try_recv().unwrap() {
                assert_eq!((frame.width, frame.height), (640, 360));
                published_while_moving += 1;
                newest_published_request = newest_published_request.max(frame.request_id);
            }
        }
        eprintln!(
            "reverse scrub published {published_while_moving} frames during 100 pointer updates; newest request {newest_published_request}"
        );
        assert!(
            published_while_moving >= 10,
            "reverse scrub published only {published_while_moving} frames during 100 pointer updates"
        );
        assert!(
            newest_published_request >= 895,
            "reverse scrub lagged more than five pointer updates behind; newest was {newest_published_request}"
        );
    }
}
