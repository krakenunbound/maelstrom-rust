//! Latest-wins in-process FFmpeg monitor decoding.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(test)]
use std::sync::{Condvar, MutexGuard};

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
const MAX_SCRUB_CACHE_INDEX_ENTRIES: usize = 1_024;
const MAX_CACHE_STREAM_STATES: usize = 4_096;
const MAX_SOURCE_ACTOR_CLIENTS: usize = 64;
pub const DEFAULT_FRAME_CACHE_BYTES: usize = 1024 * 1024 * 1024;

// Kept entirely out of production builds. This stops one exact request at the worker boundary
// without adding a runtime hook or timing dependency to the decoder.
#[cfg(test)]
struct TestDecodeBarrier {
    request_id: u64,
    path: PathBuf,
    started: (Mutex<bool>, Condvar),
    released: (Mutex<bool>, Condvar),
}

#[cfg(test)]
struct TestDecodeBarrierGuard {
    barrier: Arc<TestDecodeBarrier>,
    _serial: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestDecodeBarrierGuard {
    fn drop(&mut self) {
        *test_decode_barrier_slot()
            .lock()
            .expect("test decode barrier slot lock") = None;
        self.release();
    }
}

#[cfg(test)]
fn test_decode_barrier_slot() -> &'static Mutex<Option<Arc<TestDecodeBarrier>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<TestDecodeBarrier>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn test_decode_barrier_serial() -> &'static Mutex<()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
fn install_test_decode_barrier(request_id: u64, path: PathBuf) -> TestDecodeBarrierGuard {
    let serial = test_decode_barrier_serial()
        .lock()
        .expect("test decode barrier serial lock");
    let barrier = Arc::new(TestDecodeBarrier {
        request_id,
        path,
        started: (Mutex::new(false), Condvar::new()),
        released: (Mutex::new(false), Condvar::new()),
    });
    *test_decode_barrier_slot()
        .lock()
        .expect("test decode barrier slot lock") = Some(Arc::clone(&barrier));
    TestDecodeBarrierGuard {
        barrier,
        _serial: serial,
    }
}

#[cfg(test)]
impl TestDecodeBarrierGuard {
    fn wait_until_blocked(&self) {
        let (started, wake) = &self.barrier.started;
        let started = started.lock().expect("test decode barrier start lock");
        let (started, _) = wake
            .wait_timeout_while(started, Duration::from_secs(2), |started| !*started)
            .expect("test decode barrier start wait");
        assert!(*started, "decoder did not reach deterministic test barrier");
    }

    fn release(&self) {
        let (released, wake) = &self.barrier.released;
        *released.lock().expect("test decode barrier release lock") = true;
        wake.notify_all();
    }
}

#[cfg(test)]
fn block_test_decode_request(request: &DecodeRequest) {
    let barrier = test_decode_barrier_slot()
        .lock()
        .expect("test decode barrier slot lock")
        .as_ref()
        .filter(|barrier| barrier.request_id == request.request_id && barrier.path == request.path)
        .cloned();
    let Some(barrier) = barrier else {
        return;
    };
    let (started, wake) = &barrier.started;
    *started.lock().expect("test decode barrier start lock") = true;
    wake.notify_all();
    let (released, wake) = &barrier.released;
    let released = released.lock().expect("test decode barrier release lock");
    let _released = wake
        .wait_while(released, |released| !*released)
        .expect("test decode barrier release wait");
}

/// A cloneable, hard-capped permit pool for monitor decoder sessions.
///
/// Foreground permits are reserved for each decoder's sequential lane. Background permits are
/// used only for paused prewarm and reverse-scrub lanes, so speculative work cannot consume the
/// foreground budget.
#[derive(Clone)]
pub struct MonitorSessionPool {
    state: Arc<Mutex<MonitorSessionPoolState>>,
}

#[derive(Debug)]
struct MonitorSessionPoolState {
    active_foreground: usize,
    active_background: usize,
    peak_sticky_sessions: usize,
    foreground_cap: usize,
    background_cap: usize,
}

/// An exact, coherent snapshot of one shared monitor-session pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorSessionPoolDiagnostics {
    pub active_sticky_sessions: usize,
    pub peak_sticky_sessions: usize,
    pub session_cap: usize,
    pub active_foreground_sessions: usize,
    pub foreground_session_cap: usize,
    pub active_background_sessions: usize,
    pub background_session_cap: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorSessionLane {
    Foreground,
    Background,
}

struct MonitorSessionPermit {
    pool: MonitorSessionPool,
    lane: MonitorSessionLane,
}

impl MonitorSessionPool {
    /// Creates a pool with separate hard caps for foreground and speculative background work.
    pub fn new(foreground_session_cap: usize, background_session_cap: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(MonitorSessionPoolState {
                active_foreground: 0,
                active_background: 0,
                peak_sticky_sessions: 0,
                foreground_cap: foreground_session_cap,
                background_cap: background_session_cap,
            })),
        }
    }

    /// Returns an exact coherent snapshot; no fields are independently sampled.
    pub fn diagnostics(&self) -> MonitorSessionPoolDiagnostics {
        let state = self.state.lock().expect("monitor session pool lock");
        let active_sticky_sessions = state.active_foreground + state.active_background;
        MonitorSessionPoolDiagnostics {
            active_sticky_sessions,
            peak_sticky_sessions: state.peak_sticky_sessions.max(active_sticky_sessions),
            session_cap: state.foreground_cap + state.background_cap,
            active_foreground_sessions: state.active_foreground,
            foreground_session_cap: state.foreground_cap,
            active_background_sessions: state.active_background,
            background_session_cap: state.background_cap,
        }
    }

    fn try_acquire(&self, lane: MonitorSessionLane) -> Option<MonitorSessionPermit> {
        let mut state = self.state.lock().expect("monitor session pool lock");
        match lane {
            MonitorSessionLane::Foreground if state.active_foreground < state.foreground_cap => {
                state.active_foreground += 1;
            }
            MonitorSessionLane::Background if state.active_background < state.background_cap => {
                state.active_background += 1;
            }
            _ => return None,
        }
        state.peak_sticky_sessions = state
            .peak_sticky_sessions
            .max(state.active_foreground + state.active_background);
        Some(MonitorSessionPermit {
            pool: self.clone(),
            lane,
        })
    }
}

impl Drop for MonitorSessionPermit {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().expect("monitor session pool lock");
        let active = match self.lane {
            MonitorSessionLane::Foreground => &mut state.active_foreground,
            MonitorSessionLane::Background => &mut state.active_background,
        };
        debug_assert!(*active > 0, "monitor session permit released exactly once");
        *active = active.saturating_sub(1);
    }
}

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
    active_sticky_session_mask: AtomicUsize,
    peak_sticky_sessions: AtomicUsize,
}

impl DecoderResourceDiagnostics {
    fn new() -> Self {
        Self {
            active_sticky_session_mask: AtomicUsize::new(0),
            peak_sticky_sessions: AtomicUsize::new(0),
        }
    }

    fn snapshot(&self, cache: MonitorFrameCachePoolDiagnostics) -> MonitorDecoderDiagnostics {
        let active_sticky_sessions = self
            .active_sticky_session_mask
            .load(Ordering::Acquire)
            .count_ones() as usize;
        MonitorDecoderDiagnostics {
            frame_cache_capacity_bytes: cache.capacity_bytes,
            current_frame_cache_bytes: cache.current_bytes,
            peak_frame_cache_bytes: cache.peak_bytes,
            active_sticky_sessions,
            peak_sticky_sessions: self
                .peak_sticky_sessions
                .load(Ordering::Acquire)
                .max(active_sticky_sessions),
            session_cap: MONITOR_WORKER_COUNT,
        }
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

/// Exact, shared decoded-frame cache usage. A snapshot represents one physical cache, even
/// when several monitor decoders reference its pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorFrameCachePoolDiagnostics {
    pub capacity_bytes: usize,
    pub current_bytes: usize,
    pub peak_bytes: usize,
    pub eviction_count: u64,
}

struct FrameCacheDiagnostics {
    capacity_bytes: usize,
    current_bytes: AtomicUsize,
    peak_bytes: AtomicUsize,
    eviction_count: AtomicU64,
}

impl FrameCacheDiagnostics {
    fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            current_bytes: AtomicUsize::new(0),
            peak_bytes: AtomicUsize::new(0),
            eviction_count: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> MonitorFrameCachePoolDiagnostics {
        let current_bytes = self.current_bytes.load(Ordering::Acquire);
        MonitorFrameCachePoolDiagnostics {
            capacity_bytes: self.capacity_bytes,
            current_bytes,
            peak_bytes: self.peak_bytes.load(Ordering::Acquire).max(current_bytes),
            eviction_count: self.eviction_count.load(Ordering::Acquire),
        }
    }

    fn publish(&self, used_bytes: usize, eviction_count: u64) {
        debug_assert!(used_bytes <= self.capacity_bytes);
        self.peak_bytes.fetch_max(used_bytes, Ordering::AcqRel);
        self.current_bytes.store(used_bytes, Ordering::Release);
        self.eviction_count.store(eviction_count, Ordering::Release);
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
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
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
    /// Exact local source-frame duration, derived from CFR timing or adjacent VFR timestamps.
    /// When known, scrub cache lookup may reuse the first frame no more than one source frame
    /// after the requested timestamp. `None` permits exact matches only; it must not invent a
    /// constant-rate tolerance when timing is unknown or the final indexed VFR span is unavailable.
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

/// Cloneable ownership of one hard-capped decoded-frame cache.
///
/// Cloned pools share decoded RGBA storage only. Decoder queues, events, generations, and
/// sticky sessions remain per-decoder.
#[derive(Clone)]
pub struct MonitorFrameCachePool {
    cache: Arc<Mutex<MonitorFrameCache>>,
    diagnostics: Arc<FrameCacheDiagnostics>,
}

impl MonitorFrameCachePool {
    /// Creates one application-wide decoded-frame cache with the supplied hard byte cap.
    pub fn new(capacity_bytes: usize) -> Self {
        Self::new_internal(capacity_bytes)
    }

    /// Returns exact usage for this physical cache. Do not sum this snapshot per decoder.
    pub fn diagnostics(&self) -> MonitorFrameCachePoolDiagnostics {
        self.diagnostics.snapshot()
    }

    fn new_private(capacity_bytes: usize) -> Self {
        Self::new_internal(capacity_bytes)
    }

    fn new_internal(capacity_bytes: usize) -> Self {
        let diagnostics = Arc::new(FrameCacheDiagnostics::new(capacity_bytes));
        Self {
            cache: Arc::new(Mutex::new(MonitorFrameCache::new_with_diagnostics(
                capacity_bytes,
                Arc::clone(&diagnostics),
            ))),
            diagnostics,
        }
    }
}

/// Exact runtime ownership counts for a shared source-actor coordinator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorSourceCoordinatorDiagnostics {
    pub live_source_groups: usize,
    pub source_group_cap: usize,
    pub live_lane_actors: usize,
    pub lane_actor_cap: usize,
    /// Actors signalled for asynchronous join; this is included in the bounded total budget.
    pub retiring_lane_actors: usize,
}

/// Bounded, thread-confined ownership of sticky monitor source sessions.
///
/// The coordinator never moves FFmpeg contexts between threads.  A lazily-created lane actor
/// owns its `StickyMonitor` on one thread; decoder endpoints retain only a weakly-indexed lease.
/// Dropping the final endpoint lease shuts the actor down and returns its session permit.
#[derive(Clone)]
pub struct MonitorSourceCoordinator {
    state: Arc<Mutex<MonitorSourceCoordinatorState>>,
    session_pool: MonitorSessionPool,
    reaper: Arc<SourceActorReaper>,
}

struct MonitorSourceCoordinatorState {
    source_group_cap: usize,
    groups: HashMap<MonitorSourceKey, SourceActorGroup>,
}

struct SourceActorGroup {
    foreground: std::sync::Weak<SourceLaneActor>,
    background: std::sync::Weak<SourceLaneActor>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MonitorSourceKey {
    media_id: u32,
    path: PathBuf,
    acceleration: AccelerationPreference,
}

impl MonitorSourceKey {
    fn from_request(request: &DecodeRequest) -> Self {
        Self {
            media_id: request.media_id,
            path: request.path.clone(),
            acceleration: request.acceleration,
        }
    }
}

struct SourceLaneActor {
    shared: Arc<SourceLaneActorShared>,
    scheduler: Mutex<Option<JoinHandle<()>>>,
    reaper: Arc<SourceActorReaper>,
}

struct SourceLaneActorShared {
    pending: Mutex<VecDeque<(u64, std::sync::Weak<CoordinatorClient>)>>,
    queued_clients: Mutex<HashMap<u64, ()>>,
    wake: SyncSender<()>,
    shutdown: AtomicBool,
}

struct CoordinatorEndpoint {
    /// Serializes command publication and source-actor lease handoff. Actor decode paths never
    /// take this lock, so release cannot wait on an in-flight FFmpeg call while holding it.
    control: Mutex<()>,
    events: Arc<EventSlot>,
    stage_timings: Arc<DecoderStageTimingAccumulators>,
    frame_cache: Arc<Mutex<MonitorFrameCache>>,
    resource_diagnostics: Arc<DecoderResourceDiagnostics>,
    active: Mutex<Option<CoordinatorLease>>,
    deferred_request: Mutex<Option<DeferredDecodeRequest>>,
}

#[derive(Clone)]
struct DeferredDecodeRequest {
    request: DecodeRequest,
    speculative: bool,
}

/// One lease-specific command slot. An actor only ever consumes this client, never the mutable
/// endpoint-wide command slot, so a retiring actor cannot steal a newer source request.
struct CoordinatorClient {
    id: u64,
    worker_index: usize,
    commands: Arc<Mutex<Option<MonitorCommand>>>,
    endpoint: std::sync::Weak<CoordinatorEndpoint>,
}

struct CoordinatorLease {
    key: MonitorSourceKey,
    lane: MonitorSessionLane,
    actor: Arc<SourceLaneActor>,
    client: Arc<CoordinatorClient>,
    /// Retains the exact latest command even after the actor has consumed its command slot.
    request: DecodeRequest,
    /// True only for paused prewarm work. Reverse scrub lanes are visible work and stay protected.
    speculative: bool,
}

/// One fixed reaper thread joins retired actors off the request/control path. Every live or
/// retiring actor retains one reservation until its thread has joined, so lane churn cannot grow
/// an unbounded retirement queue or thread count.
struct SourceActorReaper {
    sender: mpsc::Sender<JoinHandle<()>>,
    state: Arc<SourceActorReaperState>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct SourceActorReaperState {
    retiring: AtomicUsize,
    reserved: AtomicUsize,
    capacity: usize,
    shutdown: AtomicBool,
}

#[derive(Debug)]
enum SourceActorAcquireError {
    Capacity,
    Spawn(String),
}

static NEXT_COORDINATOR_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

fn initialize_ffmpeg() -> Result<(), String> {
    static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
    INITIALIZED
        .get_or_init(|| ffmpeg::init().map_err(|error| error.to_string()))
        .clone()
}

impl MonitorSourceCoordinator {
    /// Creates one source-actor registry. Zero is a valid hard cap: requests defer rather than
    /// spawning an unbounded decoder thread or FFmpeg context.
    pub fn new(max_active_source_groups: usize, session_pool: MonitorSessionPool) -> Self {
        let reaper = SourceActorReaper::spawn(max_active_source_groups.saturating_mul(2));
        Self {
            state: Arc::new(Mutex::new(MonitorSourceCoordinatorState {
                source_group_cap: max_active_source_groups,
                groups: HashMap::new(),
            })),
            session_pool,
            reaper,
        }
    }

    pub fn diagnostics(&self) -> MonitorSourceCoordinatorDiagnostics {
        let mut state = self.state.lock().expect("monitor source coordinator lock");
        state.groups.retain(|_, group| {
            group.foreground.strong_count() != 0 || group.background.strong_count() != 0
        });
        let live_lane_actors = state
            .groups
            .values()
            .map(|group| {
                usize::from(group.foreground.strong_count() != 0)
                    + usize::from(group.background.strong_count() != 0)
            })
            .sum();
        MonitorSourceCoordinatorDiagnostics {
            live_source_groups: state.groups.len(),
            source_group_cap: state.source_group_cap,
            live_lane_actors,
            lane_actor_cap: state.source_group_cap.saturating_mul(2),
            retiring_lane_actors: self.reaper.retiring(),
        }
    }

    pub fn session_pool(&self) -> MonitorSessionPool {
        self.session_pool.clone()
    }

    fn acquire(
        &self,
        request: &DecodeRequest,
        lane: MonitorSessionLane,
    ) -> Result<Arc<SourceLaneActor>, SourceActorAcquireError> {
        let key = MonitorSourceKey::from_request(request);
        let mut state = self.state.lock().expect("monitor source coordinator lock");
        state.groups.retain(|_, group| {
            group.foreground.strong_count() != 0 || group.background.strong_count() != 0
        });
        if !state.groups.contains_key(&key) && state.groups.len() >= state.source_group_cap {
            return Err(SourceActorAcquireError::Capacity);
        }
        let group = state
            .groups
            .entry(key.clone())
            .or_insert_with(|| SourceActorGroup {
                foreground: std::sync::Weak::new(),
                background: std::sync::Weak::new(),
            });
        let slot = match lane {
            MonitorSessionLane::Foreground => &mut group.foreground,
            MonitorSessionLane::Background => &mut group.background,
        };
        if let Some(actor) = slot.upgrade() {
            return Ok(actor);
        }
        let actor = SourceLaneActor::spawn(
            key,
            lane,
            self.session_pool.clone(),
            Arc::clone(&self.reaper),
        )?;
        *slot = Arc::downgrade(&actor);
        Ok(actor)
    }
}

impl SourceActorReaper {
    fn spawn(capacity: usize) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel::<JoinHandle<()>>();
        let state = Arc::new(SourceActorReaperState {
            retiring: AtomicUsize::new(0),
            reserved: AtomicUsize::new(0),
            capacity,
            shutdown: AtomicBool::new(false),
        });
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("maelstrom-monitor-source-reaper".to_owned())
            .spawn(move || {
                while !thread_state.shutdown.load(Ordering::Acquire) {
                    match receiver.recv_timeout(POLL_INTERVAL) {
                        Ok(handle) => {
                            let _ = handle.join();
                            thread_state.retiring.fetch_sub(1, Ordering::AcqRel);
                            thread_state.reserved.fetch_sub(1, Ordering::AcqRel);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                while let Ok(handle) = receiver.try_recv() {
                    let _ = handle.join();
                    thread_state.retiring.fetch_sub(1, Ordering::AcqRel);
                    thread_state.reserved.fetch_sub(1, Ordering::AcqRel);
                }
            })
            .expect("failed to start monitor source reaper");
        Arc::new(Self {
            sender,
            state,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn try_reserve(&self) -> bool {
        let mut current = self.state.reserved.load(Ordering::Acquire);
        loop {
            if current >= self.state.capacity {
                return false;
            }
            match self.state.reserved.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release_reservation(&self) {
        self.state.reserved.fetch_sub(1, Ordering::AcqRel);
    }

    fn retire(&self, handle: JoinHandle<()>) {
        self.state.retiring.fetch_add(1, Ordering::AcqRel);
        // The admission check bounds outstanding retirements, so this unbounded channel has a
        // bounded producer count. It keeps the hot path free of joins or a full-channel wait.
        let _ = self.sender.send(handle);
    }

    fn retiring(&self) -> usize {
        self.state.retiring.load(Ordering::Acquire)
    }
}

impl Drop for SourceActorReaper {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self
            .thread
            .lock()
            .expect("source actor reaper thread lock")
            .take()
        {
            let _ = thread.join();
        }
    }
}

impl SourceLaneActor {
    fn spawn(
        key: MonitorSourceKey,
        lane: MonitorSessionLane,
        session_pool: MonitorSessionPool,
        reaper: Arc<SourceActorReaper>,
    ) -> Result<Arc<Self>, SourceActorAcquireError> {
        if !reaper.try_reserve() {
            return Err(SourceActorAcquireError::Capacity);
        }
        let (wake, wake_rx) = mpsc::sync_channel(1);
        let shared = Arc::new(SourceLaneActorShared {
            pending: Mutex::new(VecDeque::new()),
            queued_clients: Mutex::new(HashMap::new()),
            wake,
            shutdown: AtomicBool::new(false),
        });
        let actor = Arc::new(Self {
            shared: Arc::clone(&shared),
            scheduler: Mutex::new(None),
            reaper,
        });
        let thread_shared = Arc::clone(&shared);
        let scheduler = match thread::Builder::new()
            .name("maelstrom-monitor-source-actor".to_owned())
            .spawn(move || source_lane_actor_loop(key, lane, wake_rx, thread_shared, session_pool))
        {
            Ok(scheduler) => scheduler,
            Err(error) => {
                actor.reaper.release_reservation();
                return Err(SourceActorAcquireError::Spawn(format!(
                    "failed to start monitor source actor: {error}"
                )));
            }
        };
        *actor.scheduler.lock().expect("source actor scheduler lock") = Some(scheduler);
        Ok(actor)
    }

    fn submit(&self, client: &Arc<CoordinatorClient>) -> Result<(), DecoderClosed> {
        prune_dead_clients(&self.shared);
        {
            let mut queued = self
                .shared
                .queued_clients
                .lock()
                .expect("source actor queued client lock");
            if !queued.contains_key(&client.id) {
                if queued.len() >= MAX_SOURCE_ACTOR_CLIENTS {
                    return Err(DecoderClosed::SourceCapacityDeferred);
                }
                queued.insert(client.id, ());
                self.shared
                    .pending
                    .lock()
                    .expect("source actor pending lock")
                    .push_back((client.id, Arc::downgrade(client)));
            }
        }
        match self.shared.wake.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(())) => Err(DecoderClosed::Closed),
        }
    }
}

impl Drop for SourceLaneActor {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        let _ = self.shared.wake.try_send(());
        if let Some(scheduler) = self
            .scheduler
            .lock()
            .expect("source actor scheduler lock")
            .take()
        {
            self.reaper.retire(scheduler);
        }
    }
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
    frame_cache_pool: MonitorFrameCachePool,
    session_pool: MonitorSessionPool,
    source_coordinator: Option<MonitorSourceCoordinator>,
}

struct MonitorWorker {
    commands: Arc<Mutex<Option<MonitorCommand>>>,
    wake: Option<SyncSender<()>>,
    scheduler: Option<JoinHandle<()>>,
    endpoint: Option<Arc<CoordinatorEndpoint>>,
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
        Self::new_with_notifier_and_cache_bytes_and_session_pool(
            notify,
            frame_cache_bytes,
            // Preserve the existing per-decoder ownership budget for callers that do not opt
            // into an application-wide pool: lane zero plus three speculative lanes.
            MonitorSessionPool::new(1, MONITOR_WORKER_COUNT - 1),
        )
    }

    /// Creates a monitor decoder whose sticky sessions draw permits from a shared pool.
    pub fn new_with_notifier_and_cache_bytes_and_session_pool(
        notify: impl Fn() + Send + Sync + 'static,
        frame_cache_bytes: usize,
        session_pool: MonitorSessionPool,
    ) -> Self {
        Self::new_with_notifier_and_frame_cache_pool_and_session_pool(
            notify,
            MonitorFrameCachePool::new_private(frame_cache_bytes),
            session_pool,
        )
    }

    /// Creates a decoder with explicit shared decoded-frame-cache and sticky-session pools.
    pub fn new_with_notifier_and_frame_cache_pool_and_session_pool(
        notify: impl Fn() + Send + Sync + 'static,
        frame_cache_pool: MonitorFrameCachePool,
        session_pool: MonitorSessionPool,
    ) -> Self {
        let events = Arc::new(EventSlot::new(notify));
        let cache_reset_generation = Arc::new(AtomicU64::new(0));
        let resource_diagnostics = Arc::new(DecoderResourceDiagnostics::new());
        let frame_cache = Arc::clone(&frame_cache_pool.cache);
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
            let scheduler_session_pool = session_pool.clone();
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
                        scheduler_session_pool,
                        index,
                    )
                })
                .expect("failed to start monitor decoder scheduler");
            workers.push(MonitorWorker {
                commands,
                wake: Some(wake),
                scheduler: Some(scheduler),
                endpoint: None,
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
            frame_cache_pool,
            session_pool,
            source_coordinator: None,
        }
    }

    /// Creates a decoder that routes its four logical lanes through a shared, bounded source
    /// coordinator. Existing constructors retain their independent scheduler ownership.
    pub fn new_with_notifier_and_frame_cache_pool_and_source_coordinator(
        notify: impl Fn() + Send + Sync + 'static,
        frame_cache_pool: MonitorFrameCachePool,
        source_coordinator: MonitorSourceCoordinator,
    ) -> Self {
        let events = Arc::new(EventSlot::new(notify));
        let cache_reset_generation = Arc::new(AtomicU64::new(0));
        let resource_diagnostics = Arc::new(DecoderResourceDiagnostics::new());
        let mut workers = Vec::with_capacity(MONITOR_WORKER_COUNT);
        let mut stage_timings = Vec::with_capacity(MONITOR_WORKER_COUNT);
        for _ in 0..MONITOR_WORKER_COUNT {
            let commands = Arc::new(Mutex::new(None));
            let lane_stage_timings = Arc::new(DecoderStageTimingAccumulators::default());
            workers.push(MonitorWorker {
                commands: Arc::clone(&commands),
                wake: None,
                scheduler: None,
                endpoint: Some(Arc::new(CoordinatorEndpoint {
                    control: Mutex::new(()),
                    events: Arc::clone(&events),
                    stage_timings: Arc::clone(&lane_stage_timings),
                    frame_cache: Arc::clone(&frame_cache_pool.cache),
                    resource_diagnostics: Arc::clone(&resource_diagnostics),
                    active: Mutex::new(None),
                    deferred_request: Mutex::new(None),
                })),
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
            frame_cache_pool,
            session_pool: source_coordinator.session_pool(),
            source_coordinator: Some(source_coordinator),
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
                let mut outcome = Ok(());
                for index in 0..self.workers.len() {
                    let mut lane_request = request.clone();
                    lane_request.prewarm_scrub_workers = false;
                    match self.send_request_to(index, lane_request, index != 0) {
                        Err(DecoderClosed::Closed) => outcome = Err(DecoderClosed::Closed),
                        Err(DecoderClosed::SourceCapacityDeferred)
                            if outcome != Err(DecoderClosed::Closed) =>
                        {
                            outcome = Err(DecoderClosed::SourceCapacityDeferred)
                        }
                        _ => {}
                    }
                }
                return outcome;
            }
            return self.send_request_to(0, request, false);
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
        self.send_request_to(index, request, false)
    }

    /// Clears queued work while retaining open media contexts for reuse.
    pub fn cancel_pending(&self) -> Result<(), DecoderClosed> {
        *self.last_scrub_target.lock().expect("scrub target lock") = None;
        let mut closed = false;
        for index in 0..self.workers.len() {
            closed |= self.send_to(index, MonitorCommand::Cancel).is_err();
        }
        if closed {
            Err(DecoderClosed::Closed)
        } else {
            Ok(())
        }
    }

    /// Alias for [`Self::cancel_pending`].
    pub fn cancel(&self) -> Result<(), DecoderClosed> {
        self.cancel_pending()
    }

    /// Cancels active work and releases sticky decoder sessions and cached frames on workers.
    pub fn reset_live_cache(&self) -> Result<(), DecoderClosed> {
        self.cache_reset_generation.fetch_add(1, Ordering::AcqRel);
        self.release_live_sessions()?;
        self.frame_cache_pool
            .cache
            .lock()
            .expect("monitor frame cache lock")
            .clear();
        Ok(())
    }

    /// Releases sticky decoder/source-actor sessions without clearing decoded-frame cache data.
    pub fn release_live_sessions(&self) -> Result<(), DecoderClosed> {
        *self.last_scrub_target.lock().expect("scrub target lock") = None;
        let mut closed = false;
        for index in 0..self.workers.len() {
            closed |= self.send_to(index, MonitorCommand::Release).is_err();
        }
        if closed {
            Err(DecoderClosed::Closed)
        } else {
            Ok(())
        }
    }

    /// Yields live source-actor leases while retaining each lane's exact latest request for an
    /// explicit later [`Self::retry_deferred_requests`]. This never joins an actor on the caller
    /// thread. Explicit [`Self::release_live_sessions`] does not create retry work.
    ///
    /// Without the opt-in shared source coordinator there is no shared source-group capacity to
    /// yield, so this is equivalent to [`Self::release_live_sessions`].
    pub fn defer_live_sessions(&self) -> Result<bool, DecoderClosed> {
        *self.last_scrub_target.lock().expect("scrub target lock") = None;
        if self.source_coordinator.is_none() {
            self.release_live_sessions()?;
            return Ok(false);
        }
        let mut closed = false;
        let mut deferred_live_lease = false;
        for (index, worker) in self.workers.iter().enumerate() {
            let Some(endpoint) = worker.endpoint.as_ref() else {
                closed |= self.send_to(index, MonitorCommand::Release).is_err();
                continue;
            };
            let _control = endpoint
                .control
                .lock()
                .expect("coordinator endpoint control lock");
            let lease = endpoint
                .active
                .lock()
                .expect("coordinator endpoint lease lock")
                .take();
            if let Some(lease) = lease {
                deferred_live_lease = true;
                *endpoint
                    .deferred_request
                    .lock()
                    .expect("coordinator deferred request lock") = Some(DeferredDecodeRequest {
                    request: lease.request.clone(),
                    speculative: lease.speculative,
                });
                *lease.client.commands.lock().expect("monitor command lock") =
                    Some(MonitorCommand::Release);
                drop(lease);
            }
            endpoint
                .resource_diagnostics
                .publish_worker_session(index, false);
        }
        if closed {
            Err(DecoderClosed::Closed)
        } else {
            Ok(deferred_live_lease)
        }
    }

    /// Releases every speculative background lane and discards its deferred retry without
    /// disturbing the foreground lane. Callers use this as the first reclamation step before
    /// yielding any visible source. Actor shutdown and join remain asynchronous.
    pub fn release_speculative_sessions(&self) -> Result<bool, DecoderClosed> {
        if self.source_coordinator.is_none() {
            return Ok(false);
        }

        let mut released_live_lease = false;
        for (index, worker) in self.workers.iter().enumerate().skip(1) {
            let Some(endpoint) = worker.endpoint.as_ref() else {
                continue;
            };
            let _control = endpoint
                .control
                .lock()
                .expect("coordinator endpoint control lock");
            let mut deferred = endpoint
                .deferred_request
                .lock()
                .expect("coordinator deferred request lock");
            if deferred.as_ref().is_some_and(|request| request.speculative) {
                deferred.take();
            }
            drop(deferred);
            let mut active = endpoint
                .active
                .lock()
                .expect("coordinator endpoint lease lock");
            let lease = active
                .as_ref()
                .is_some_and(|lease| lease.speculative)
                .then(|| active.take())
                .flatten();
            drop(active);
            if let Some(lease) = lease {
                released_live_lease = true;
                *lease.client.commands.lock().expect("monitor command lock") =
                    Some(MonitorCommand::Release);
                drop(lease);
                endpoint
                    .resource_diagnostics
                    .publish_worker_session(index, false);
            }
        }
        Ok(released_live_lease)
    }

    /// Retries newest requests explicitly deferred by the bounded source coordinator. Call this
    /// after another monitor frees a source group; a repeated capacity result keeps each latest
    /// request retained for a later retry.
    pub fn retry_deferred_requests(&self) -> Result<(), DecoderClosed> {
        let mut deferred = None;
        for (index, worker) in self.workers.iter().enumerate() {
            let Some(endpoint) = worker.endpoint.as_ref() else {
                continue;
            };
            let request = endpoint
                .deferred_request
                .lock()
                .expect("coordinator deferred request lock")
                .clone();
            if let Some(request) = request {
                if let Err(error) =
                    self.send_request_to(index, request.request, request.speculative)
                {
                    deferred = Some(error);
                }
            }
        }
        deferred.map_or(Ok(()), Err)
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
        self.resource_diagnostics
            .snapshot(self.frame_cache_pool.diagnostics())
    }

    /// Returns the decoded-frame pool used by this decoder.
    pub fn frame_cache_pool(&self) -> MonitorFrameCachePool {
        self.frame_cache_pool.clone()
    }

    /// Returns the shared pool used by this decoder. Diagnostics on the returned clone are
    /// application-wide when the explicit shared-pool constructor was used.
    pub fn session_pool(&self) -> MonitorSessionPool {
        self.session_pool.clone()
    }

    /// Returns source-actor ownership only when this decoder was created with the opt-in
    /// coordinator constructor.
    pub fn source_coordinator(&self) -> Option<MonitorSourceCoordinator> {
        self.source_coordinator.clone()
    }

    fn send_request_to(
        &self,
        index: usize,
        request: DecodeRequest,
        speculative: bool,
    ) -> Result<(), DecoderClosed> {
        self.send_to_with_role(index, MonitorCommand::Request(request), speculative)
    }

    fn send_to(&self, index: usize, command: MonitorCommand) -> Result<(), DecoderClosed> {
        self.send_to_with_role(index, command, false)
    }

    fn send_to_with_role(
        &self,
        index: usize,
        command: MonitorCommand,
        speculative: bool,
    ) -> Result<(), DecoderClosed> {
        let worker = &self.workers[index];
        if let (Some(coordinator), Some(endpoint)) = (&self.source_coordinator, &worker.endpoint) {
            let _control = endpoint
                .control
                .lock()
                .expect("coordinator endpoint control lock");
            if matches!(command, MonitorCommand::Release | MonitorCommand::Shutdown) {
                let lease = endpoint
                    .active
                    .lock()
                    .expect("coordinator endpoint lease lock")
                    .take();
                if let Some(lease) = lease.as_ref() {
                    *lease.client.commands.lock().expect("monitor command lock") = Some(command);
                }
                drop(lease);
                endpoint
                    .resource_diagnostics
                    .publish_worker_session(index, false);
                return Ok(());
            }
            if matches!(command, MonitorCommand::Cancel) {
                endpoint
                    .deferred_request
                    .lock()
                    .expect("coordinator deferred request lock")
                    .take();
                let lease = endpoint
                    .active
                    .lock()
                    .expect("coordinator endpoint lease lock")
                    .as_ref()
                    .map(|lease| Arc::clone(&lease.client));
                if let Some(client) = lease {
                    *client.commands.lock().expect("monitor command lock") =
                        Some(MonitorCommand::Cancel);
                    return endpoint
                        .active
                        .lock()
                        .expect("coordinator endpoint lease lock")
                        .as_ref()
                        .expect("active client retained for cancel")
                        .actor
                        .submit(&client);
                }
                return Ok(());
            }
            let MonitorCommand::Request(request) = command else {
                return Ok(());
            };
            let lane = if index == 0 {
                MonitorSessionLane::Foreground
            } else {
                MonitorSessionLane::Background
            };
            let key = MonitorSourceKey::from_request(&request);
            let (actor, client, previous) = {
                let mut active = endpoint
                    .active
                    .lock()
                    .expect("coordinator endpoint lease lock");
                match active.as_ref() {
                    Some(lease) if lease.key == key && lease.lane == lane => (
                        Some(Arc::clone(&lease.actor)),
                        Some(Arc::clone(&lease.client)),
                        None,
                    ),
                    _ => (None, None, active.take()),
                }
            };
            if let Some(lease) = previous.as_ref() {
                *lease.client.commands.lock().expect("monitor command lock") =
                    Some(MonitorCommand::Release);
            }
            drop(previous);
            let actor = match actor {
                Some(actor) => actor,
                None => match coordinator.acquire(&request, lane) {
                    Ok(actor) => actor,
                    Err(SourceActorAcquireError::Capacity) => {
                        *endpoint
                            .deferred_request
                            .lock()
                            .expect("coordinator deferred request lock") =
                            Some(DeferredDecodeRequest {
                                request,
                                speculative,
                            });
                        endpoint
                            .resource_diagnostics
                            .publish_worker_session(index, false);
                        return Err(DecoderClosed::SourceCapacityDeferred);
                    }
                    Err(SourceActorAcquireError::Spawn(message)) => {
                        endpoint
                            .deferred_request
                            .lock()
                            .expect("coordinator deferred request lock")
                            .take();
                        endpoint
                            .resource_diagnostics
                            .publish_worker_session(index, false);
                        endpoint.events.publish(DecodeEvent::Error(DecodeError {
                            project_epoch: request.project_epoch,
                            request_id: request.request_id,
                            media_id: request.media_id,
                            source_tick: request.source_tick,
                            message,
                        }));
                        return Ok(());
                    }
                },
            };
            let client = client.unwrap_or_else(|| {
                Arc::new(CoordinatorClient {
                    id: NEXT_COORDINATOR_CLIENT_ID.fetch_add(1, Ordering::Relaxed),
                    worker_index: index,
                    commands: Arc::new(Mutex::new(None)),
                    endpoint: Arc::downgrade(endpoint),
                })
            });
            let retained_request = request.clone();
            *client.commands.lock().expect("monitor command lock") =
                Some(MonitorCommand::Request(request));
            endpoint
                .deferred_request
                .lock()
                .expect("coordinator deferred request lock")
                .take();
            let mut active = endpoint
                .active
                .lock()
                .expect("coordinator endpoint lease lock");
            match active.as_mut() {
                Some(lease) => {
                    lease.request = retained_request.clone();
                    lease.speculative = speculative;
                }
                None => {
                    *active = Some(CoordinatorLease {
                        key,
                        lane,
                        actor: Arc::clone(&actor),
                        client: Arc::clone(&client),
                        request: retained_request.clone(),
                        speculative,
                    });
                }
            }
            drop(active);
            if let Err(error) = actor.submit(&client) {
                if error == DecoderClosed::SourceCapacityDeferred {
                    *endpoint
                        .deferred_request
                        .lock()
                        .expect("coordinator deferred request lock") =
                        Some(DeferredDecodeRequest {
                            request: retained_request,
                            speculative,
                        });
                    return Err(error);
                }
                let lease = endpoint
                    .active
                    .lock()
                    .expect("coordinator endpoint lease lock")
                    .take();
                drop(lease);
                endpoint
                    .resource_diagnostics
                    .publish_worker_session(index, false);
                return Err(error);
            }
            return Ok(());
        }
        *worker.commands.lock().expect("monitor command lock") = Some(command);
        let Some(wake) = &worker.wake else {
            return Err(DecoderClosed::Closed);
        };
        match wake.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(())) => Err(DecoderClosed::Closed),
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

/// A monitor request could not be accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderClosed {
    /// The local scheduler or source actor has terminated.
    Closed,
    /// The bounded source coordinator retained the latest request for a later retry but cannot
    /// activate a new source/lane actor until bounded capacity becomes available.
    SourceCapacityDeferred,
}

impl fmt::Display for DecoderClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("monitor decoder scheduler has stopped"),
            Self::SourceCapacityDeferred => {
                f.write_str("monitor source capacity is temporarily deferred")
            }
        }
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
    Release,
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
    cache_diagnostics: Arc<FrameCacheDiagnostics>,
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
            Arc::new(FrameCacheDiagnostics::new(capacity_bytes)),
        )
    }

    fn new_with_diagnostics(
        capacity_bytes: usize,
        cache_diagnostics: Arc<FrameCacheDiagnostics>,
    ) -> Self {
        Self {
            frames: FrameCache::new(capacity_bytes),
            cache_diagnostics,
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
        let reset_cache =
            project_changed || scaling_changed || stream_limit_reached || source_changed;
        if reset_cache {
            self.clear();
            self.project_epoch = Some(request.cache_epoch);
        }
        self.sources
            .entry(request.media_id)
            .or_insert_with(|| request.path.clone());
        self.high_quality_scaling = Some(request.high_quality_scaling);
        reset_cache
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
            .unwrap_or_default();
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
        self.cache_diagnostics
            .publish(self.frames.used_bytes(), self.frames.eviction_count());
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
    session_pool: MonitorSessionPool,
    worker_index: usize,
) {
    if initialize_ffmpeg().is_err() {
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
                Some(MonitorCommand::Release) => {
                    sessions.clear();
                    resource_diagnostics.publish_worker_session(worker_index, false);
                    continue;
                }
                Some(MonitorCommand::Shutdown) => {
                    sessions.clear();
                    resource_diagnostics.publish_worker_session(worker_index, false);
                    return;
                }
            }
        }

        let request = pending.take().expect("pending monitor request");
        #[cfg(test)]
        block_test_decode_request(&request);
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
        let mut deferred_for_capacity = false;
        let event = decode_monitor_request(
            &mut sessions,
            &frame_cache,
            &request,
            &commands,
            &mut on_progress,
            &mut on_session_state,
            &mut || deferred_for_capacity = true,
            &stage_timings,
            &session_pool,
            worker_index,
            false,
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
            Some(MonitorCommand::Release) => {
                sessions.clear();
                resource_diagnostics.publish_worker_session(worker_index, false);
            }
            Some(MonitorCommand::Shutdown) => {
                sessions.clear();
                resource_diagnostics.publish_worker_session(worker_index, false);
                return;
            }
            None => {
                if deferred_for_capacity {
                    // A saturated speculative lane must retry quietly. Waiting keeps this worker
                    // from spin-polling the permit pool while another lane remains active.
                    thread::sleep(POLL_INTERVAL);
                    pending = Some(request);
                } else if let Some(event) = event {
                    events.publish(event);
                }
            }
        }
    }
}

/// Runs one source/lane actor. The actor owns its FFmpeg contexts and consumes only weak
/// endpoint references, so inactive decoders cannot keep a source group alive.
fn source_lane_actor_loop(
    _key: MonitorSourceKey,
    _lane: MonitorSessionLane,
    wake: Receiver<()>,
    shared: Arc<SourceLaneActorShared>,
    session_pool: MonitorSessionPool,
) {
    if initialize_ffmpeg().is_err() {
        return;
    }
    let mut sessions = HashMap::<u32, StickyMonitor>::new();
    while !shared.shutdown.load(Ordering::Acquire) {
        let client = {
            let client = shared
                .pending
                .lock()
                .expect("source actor pending lock")
                .pop_front()
                .map(|(id, weak)| (id, weak.upgrade()));
            if let Some((id, _)) = client.as_ref() {
                shared
                    .queued_clients
                    .lock()
                    .expect("source actor queued client lock")
                    .remove(id);
            }
            client.and_then(|(_, client)| client)
        };
        let Some(client) = client else {
            match wake.recv_timeout(POLL_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        let Some(endpoint) = client.endpoint.upgrade() else {
            continue;
        };
        let worker_index = client.worker_index;
        if !client_is_bound_to(&endpoint, &client, &shared) {
            continue;
        }
        let command = client.commands.lock().expect("monitor command lock").take();
        let Some(MonitorCommand::Request(request)) = command else {
            continue;
        };
        #[cfg(test)]
        block_test_decode_request(&request);
        let _request_timer = StageTimer::new(&endpoint.stage_timings.worker_request);
        let progress_events = Arc::clone(&endpoint.events);
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
        let diagnostics = Arc::clone(&endpoint.resource_diagnostics);
        let mut on_session_state =
            move |active| diagnostics.publish_worker_session(worker_index, active);
        let mut deferred_for_capacity = false;
        let event = decode_monitor_request(
            &mut sessions,
            &endpoint.frame_cache,
            &request,
            &client.commands,
            &mut on_progress,
            &mut on_session_state,
            &mut || deferred_for_capacity = true,
            &endpoint.stage_timings,
            &session_pool,
            worker_index,
            true,
        );
        if shared.shutdown.load(Ordering::Acquire)
            || !client_is_bound_to(&endpoint, &client, &shared)
        {
            continue;
        }
        // Drop the command-slot guard before entering an arm. Some arms republish retained
        // work into the same slot; matching directly on the lock temporary can self-deadlock.
        let completed_command = client.commands.lock().expect("monitor command lock").take();
        match completed_command {
            Some(MonitorCommand::Request(newer)) => {
                let completed_newest = event
                    .as_ref()
                    .is_some_and(|event| event_request_id(event) == newer.request_id);
                if same_decode_generation(&request, &newer) {
                    if let Some(event) = event {
                        endpoint.events.publish(event);
                    }
                    if !completed_newest {
                        *client.commands.lock().expect("monitor command lock") =
                            Some(MonitorCommand::Request(newer));
                        let _ = enqueue_client(&shared, &client);
                    }
                } else {
                    *client.commands.lock().expect("monitor command lock") =
                        Some(MonitorCommand::Request(newer));
                    let _ = enqueue_client(&shared, &client);
                }
            }
            Some(MonitorCommand::Cancel) | Some(MonitorCommand::Release) => {}
            Some(MonitorCommand::Shutdown) => {}
            None if deferred_for_capacity => {
                *client.commands.lock().expect("monitor command lock") =
                    Some(MonitorCommand::Request(request));
                thread::sleep(POLL_INTERVAL);
                let _ = enqueue_client(&shared, &client);
            }
            None => {
                if let Some(event) = event {
                    endpoint.events.publish(event);
                }
            }
        }
    }
    sessions.clear();
}

fn enqueue_client(
    shared: &Arc<SourceLaneActorShared>,
    client: &Arc<CoordinatorClient>,
) -> Result<(), DecoderClosed> {
    prune_dead_clients(shared);
    let mut queued = shared
        .queued_clients
        .lock()
        .expect("source actor queued client lock");
    if !queued.contains_key(&client.id) {
        if queued.len() >= MAX_SOURCE_ACTOR_CLIENTS {
            return Err(DecoderClosed::SourceCapacityDeferred);
        }
        queued.insert(client.id, ());
        shared
            .pending
            .lock()
            .expect("source actor pending lock")
            .push_back((client.id, Arc::downgrade(client)));
    }
    drop(queued);
    match shared.wake.try_send(()) {
        Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
        Err(mpsc::TrySendError::Disconnected(())) => Err(DecoderClosed::Closed),
    }
}

fn prune_dead_clients(shared: &Arc<SourceLaneActorShared>) {
    let stale = {
        let mut pending = shared.pending.lock().expect("source actor pending lock");
        let stale = pending
            .iter()
            .filter_map(|(id, client)| client.upgrade().is_none().then_some(*id))
            .collect::<Vec<_>>();
        pending.retain(|(_, client)| client.strong_count() != 0);
        stale
    };
    if !stale.is_empty() {
        let mut queued = shared
            .queued_clients
            .lock()
            .expect("source actor queued client lock");
        for id in stale {
            queued.remove(&id);
        }
    }
}

fn client_is_bound_to(
    endpoint: &CoordinatorEndpoint,
    client: &CoordinatorClient,
    shared: &Arc<SourceLaneActorShared>,
) -> bool {
    endpoint
        .active
        .lock()
        .expect("coordinator endpoint lease lock")
        .as_ref()
        .is_some_and(|lease| {
            Arc::ptr_eq(&lease.actor.shared, shared) && lease.client.id == client.id
        })
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
    on_deferred_for_capacity: &mut dyn FnMut(),
    stage_timings: &DecoderStageTimingAccumulators,
    session_pool: &MonitorSessionPool,
    worker_index: usize,
    coordinator_owned_sessions: bool,
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
        && !coordinator_owned_sessions
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
            match open_sticky_monitor(
                request,
                session_pool,
                worker_index,
                coordinator_owned_sessions,
            ) {
                Ok(Some(mut session)) => {
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
                // A speculative lane must never turn global capacity pressure into a visible
                // decode error. Its next latest-wins request can retry after another session is
                // evicted or reset.
                Ok(None) => {
                    on_deferred_for_capacity();
                    return None;
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
            on_deferred_for_capacity,
            stage_timings,
            session_pool,
            worker_index,
            coordinator_owned_sessions,
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
                Some(MonitorCommand::Cancel | MonitorCommand::Release | MonitorCommand::Shutdown)
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
    on_deferred_for_capacity: &mut dyn FnMut(),
    stage_timings: &DecoderStageTimingAccumulators,
    session_pool: &MonitorSessionPool,
    worker_index: usize,
    coordinator_owned_sessions: bool,
) -> Result<Option<DecodedRgba>, String> {
    let fallback = sessions
        .get(&request.media_id)
        .and_then(|session| software_fallback_request(request, session.backend));
    let Some(open_request) = fallback else {
        return Err(hardware_error);
    };
    sessions.remove(&request.media_id);
    on_session_state(!sessions.is_empty());
    match open_sticky_monitor(
        &open_request,
        session_pool,
        worker_index,
        coordinator_owned_sessions,
    ) {
        Ok(Some(mut session)) => {
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
        Ok(None) => {
            on_deferred_for_capacity();
            Ok(None)
        }
        Err(software_error) => Err(format!(
            "hardware decoder failed ({hardware_error}); software fallback could not open ({software_error})"
        )),
    }
}

/// Acquires a permit before allocating FFmpeg contexts. Background capacity exhaustion is a
/// normal defer outcome. Foreground exhaustion is also transient for source-coordinator actors:
/// an asynchronously retiring predecessor may still own the permit. Independent schedulers keep
/// treating it as an invariant violation.
fn open_sticky_monitor(
    request: &DecodeRequest,
    session_pool: &MonitorSessionPool,
    worker_index: usize,
    coordinator_owned_sessions: bool,
) -> Result<Option<StickyMonitor>, String> {
    let lane = if worker_index == 0 {
        MonitorSessionLane::Foreground
    } else {
        MonitorSessionLane::Background
    };
    let Some(permit) = session_pool.try_acquire(lane) else {
        return match (lane, coordinator_owned_sessions) {
            (MonitorSessionLane::Background, _) | (_, true) => Ok(None),
            (MonitorSessionLane::Foreground, false) => Err(
                "monitor foreground session pool is exhausted; a foreground permit was not released"
                    .to_owned(),
            ),
        };
    };
    StickyMonitor::open(request, permit).map(Some)
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
    // Held for the full lifetime of the libav contexts. Every erase, reset, failed open, and
    // hardware-to-software replacement therefore releases capacity through RAII.
    _session_permit: MonitorSessionPermit,
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
    fn open(request: &DecodeRequest, session_permit: MonitorSessionPermit) -> Result<Self, String> {
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
            _session_permit: session_permit,
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
        sync::mpsc,
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
    fn shared_monitor_session_pool_enforces_lane_caps_and_raii_release() {
        let pool = MonitorSessionPool::new(1, 2);
        let foreground = pool
            .try_acquire(MonitorSessionLane::Foreground)
            .expect("foreground permit");
        let background_one = pool
            .try_acquire(MonitorSessionLane::Background)
            .expect("first background permit");
        let background_two = pool
            .try_acquire(MonitorSessionLane::Background)
            .expect("second background permit");
        assert!(pool.try_acquire(MonitorSessionLane::Foreground).is_none());
        assert!(pool.try_acquire(MonitorSessionLane::Background).is_none());
        assert_eq!(
            pool.diagnostics(),
            MonitorSessionPoolDiagnostics {
                active_sticky_sessions: 3,
                peak_sticky_sessions: 3,
                session_cap: 3,
                active_foreground_sessions: 1,
                foreground_session_cap: 1,
                active_background_sessions: 2,
                background_session_cap: 2,
            }
        );
        drop(background_one);
        assert_eq!(pool.diagnostics().active_background_sessions, 1);
        drop(foreground);
        drop(background_two);
        let snapshot = pool.diagnostics();
        assert_eq!(snapshot.active_sticky_sessions, 0);
        assert_eq!(snapshot.peak_sticky_sessions, 3);
    }

    #[test]
    fn shared_monitor_session_pool_snapshots_remain_coherent_under_contention() {
        let pool = MonitorSessionPool::new(2, 2);
        let barrier = Arc::new(std::sync::Barrier::new(13));
        let mut workers = Vec::new();
        for index in 0..12 {
            let worker_pool = pool.clone();
            let worker_barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                worker_barrier.wait();
                let lane = if index % 2 == 0 {
                    MonitorSessionLane::Foreground
                } else {
                    MonitorSessionLane::Background
                };
                let permit = worker_pool.try_acquire(lane);
                if permit.is_some() {
                    thread::sleep(Duration::from_millis(5));
                }
                permit.is_some()
            }));
        }
        barrier.wait();
        while workers.iter().any(|worker| !worker.is_finished()) {
            let snapshot = pool.diagnostics();
            assert_eq!(
                snapshot.active_sticky_sessions,
                snapshot.active_foreground_sessions + snapshot.active_background_sessions
            );
            assert!(snapshot.active_foreground_sessions <= snapshot.foreground_session_cap);
            assert!(snapshot.active_background_sessions <= snapshot.background_session_cap);
            assert!(snapshot.peak_sticky_sessions <= snapshot.session_cap);
            thread::yield_now();
        }
        let acquired = workers
            .into_iter()
            .filter_map(|worker| worker.join().ok())
            .filter(|acquired| *acquired)
            .count();
        assert!(acquired > 0, "at least one contended acquisition succeeds");
        let snapshot = pool.diagnostics();
        assert_eq!(snapshot.active_sticky_sessions, 0);
        assert!(snapshot.peak_sticky_sessions <= snapshot.session_cap);
    }

    #[test]
    fn source_coordinator_caps_groups_and_releases_weak_actor_entries() {
        let pool = MonitorSessionPool::new(1, 0);
        let coordinator = MonitorSourceCoordinator::new(1, pool);
        let first = request(PathBuf::from("first-source.mp4"), 1);
        let actor = coordinator
            .acquire(&first, MonitorSessionLane::Foreground)
            .expect("first source actor");
        assert_eq!(coordinator.diagnostics().live_source_groups, 1);
        let mut second = first.clone();
        second.path = PathBuf::from("second-source.mp4");
        assert!(
            coordinator
                .acquire(&second, MonitorSessionLane::Foreground)
                .is_err()
        );
        drop(actor);
        let deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.diagnostics().live_source_groups != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(coordinator.diagnostics().live_source_groups, 0);
        let second_actor = coordinator
            .acquire(&second, MonitorSessionLane::Foreground)
            .expect("released source capacity");
        drop(second_actor);
    }

    #[test]
    fn source_coordinator_reuses_one_foreground_session_across_decoder_endpoints() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let pool = MonitorSessionPool::new(1, 3);
        let cache = MonitorFrameCachePool::new(1024 * 1024);
        let coordinator = MonitorSourceCoordinator::new(2, pool.clone());
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            cache.clone(),
            coordinator.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            cache,
            coordinator.clone(),
        );
        let mut first_request = request(path.clone(), 701);
        first_request.cache_epoch = 88;
        first_request.source_tick = 100_000;
        first.request(first_request.clone()).expect("first request");
        let first_event = receive_for(&first, &first_request);
        let DecodeEvent::Frame(first_frame) = first_event else {
            panic!("first decode failed")
        };
        assert_frame_reaches_target(&first_frame, &first_request);
        let mut second_request = first_request.clone();
        second_request.request_id = 702;
        second_request.cache_epoch = 89;
        second_request.width = 80;
        second_request.height = 45;
        second_request.high_quality_scaling = !second_request.high_quality_scaling;
        second
            .request(second_request.clone())
            .expect("second request");
        let second_event = receive_for(&second, &second_request);
        let DecodeEvent::Frame(second_frame) = second_event else {
            panic!("second decode failed")
        };
        assert_frame_reaches_target(&second_frame, &second_request);
        assert_eq!(pool.diagnostics().active_foreground_sessions, 1);
        assert_eq!(coordinator.diagnostics().live_source_groups, 1);
        first.release_live_sessions().expect("first release");
        second.release_live_sessions().expect("second release");
        let deadline = Instant::now() + Duration::from_secs(2);
        while pool.diagnostics().active_sticky_sessions != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        drop(first);
        drop(second);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_coordinator_keeps_distinct_sources_and_decoder_epochs_independent() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let pool = MonitorSessionPool::new(2, 2);
        let coordinator = MonitorSourceCoordinator::new(2, pool.clone());
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let mut first_request = request(first_path.clone(), 801);
        first_request.cache_epoch = 101;
        let mut second_request = request(second_path.clone(), 901);
        second_request.cache_epoch = 102;
        second_request.media_id = 4;
        second_request.source_tick = 100_000;
        first
            .request(first_request.clone())
            .expect("first source request");
        second
            .request(second_request.clone())
            .expect("second source request");
        let DecodeEvent::Frame(first_frame) = receive_for(&first, &first_request) else {
            panic!("first source did not decode")
        };
        let DecodeEvent::Frame(second_frame) = receive_for(&second, &second_request) else {
            panic!("second source did not decode")
        };
        assert_frame_reaches_target(&first_frame, &first_request);
        assert_frame_reaches_target(&second_frame, &second_request);
        assert_eq!(pool.diagnostics().active_foreground_sessions, 2);
        assert_eq!(coordinator.diagnostics().live_source_groups, 2);
        drop(first);
        drop(second);
        let deadline = Instant::now() + Duration::from_secs(2);
        while pool.diagnostics().active_sticky_sessions != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn source_coordinator_replaces_an_endpoint_source_at_the_group_cap() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let pool = MonitorSessionPool::new(1, 3);
        let coordinator = MonitorSourceCoordinator::new(1, pool.clone());
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let mut first_request = request(first_path.clone(), 1001);
        first_request.cache_epoch = 700;
        decoder
            .request(first_request.clone())
            .expect("first source request");
        let DecodeEvent::Frame(first_frame) = receive_for(&decoder, &first_request) else {
            panic!("first source did not decode")
        };
        assert_frame_reaches_target(&first_frame, &first_request);
        assert_eq!(pool.diagnostics().active_foreground_sessions, 1);
        let mut second_request = first_request.clone();
        second_request.request_id = 1002;
        second_request.path = second_path.clone();
        second_request.source_tick = 100_000;
        decoder
            .request(second_request.clone())
            .expect("source replacement at coordinator cap");
        let DecodeEvent::Frame(second_frame) = receive_for(&decoder, &second_request) else {
            panic!("replacement source did not decode")
        };
        assert_frame_reaches_target(&second_frame, &second_request);
        assert_eq!(pool.diagnostics().active_foreground_sessions, 1);
        assert_eq!(coordinator.diagnostics().live_source_groups, 1);
        decoder.release_live_sessions().expect("release source");
        let deadline = Instant::now() + Duration::from_secs(2);
        while pool.diagnostics().active_sticky_sessions != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        drop(decoder);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn deferred_source_request_delivers_after_explicit_retry() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let pool = MonitorSessionPool::new(1, 3);
        let coordinator = MonitorSourceCoordinator::new(1, pool);
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator,
        );
        let first_request = request(first_path.clone(), 1101);
        first.request(first_request.clone()).unwrap();
        let _ = receive_for(&first, &first_request);
        let mut second_request = request(second_path.clone(), 1102);
        second_request.media_id = 4;
        assert_eq!(
            second.request(second_request.clone()),
            Err(DecoderClosed::SourceCapacityDeferred)
        );
        first.release_live_sessions().unwrap();
        second.retry_deferred_requests().unwrap();
        let DecodeEvent::Frame(frame) = receive_for(&second, &second_request) else {
            panic!("deferred source did not decode after retry")
        };
        assert_frame_reaches_target(&frame, &second_request);
        drop(first);
        drop(second);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn deferred_live_sessions_yield_capacity_and_retry_the_latest_request() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let pool = MonitorSessionPool::new(1, 3);
        let coordinator = MonitorSourceCoordinator::new(1, pool.clone());
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let mut first_request = request(first_path.clone(), 1161);
        first.request(first_request.clone()).unwrap();
        let _ = receive_for(&first, &first_request);
        first_request.request_id = 1162;
        first_request.source_tick = 100_000;
        first.request(first_request.clone()).unwrap();
        let _ = receive_for(&first, &first_request);

        assert!(first.defer_live_sessions().unwrap());
        let released_deadline = Instant::now() + Duration::from_secs(2);
        while (pool.diagnostics().active_sticky_sessions != 0
            || coordinator.diagnostics().live_source_groups != 0)
            && Instant::now() < released_deadline
        {
            thread::yield_now();
        }
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        assert_eq!(coordinator.diagnostics().live_source_groups, 0);

        let mut second_request = request(second_path.clone(), 1163);
        second_request.media_id = 4;
        second.request(second_request.clone()).unwrap();
        let _ = receive_for(&second, &second_request);
        let diagnostics = coordinator.diagnostics();
        assert!(diagnostics.live_source_groups <= diagnostics.source_group_cap);
        assert!(
            diagnostics.live_lane_actors + diagnostics.retiring_lane_actors
                <= diagnostics.lane_actor_cap
        );
        assert_eq!(
            first.retry_deferred_requests(),
            Err(DecoderClosed::SourceCapacityDeferred)
        );

        assert!(second.defer_live_sessions().unwrap());
        second.cancel_pending().unwrap();
        let second_released_deadline = Instant::now() + Duration::from_secs(2);
        while (pool.diagnostics().active_sticky_sessions != 0
            || coordinator.diagnostics().live_source_groups != 0)
            && Instant::now() < second_released_deadline
        {
            thread::yield_now();
        }
        first.retry_deferred_requests().unwrap();
        let DecodeEvent::Frame(frame) = receive_for(&first, &first_request) else {
            panic!("deferred latest request did not decode after retry")
        };
        assert_eq!(frame.request_id, first_request.request_id);
        assert!(frame.source_tick >= first_request.source_tick);

        first.release_live_sessions().unwrap();
        let final_deadline = Instant::now() + Duration::from_secs(2);
        while (pool.diagnostics().active_sticky_sessions != 0
            || coordinator.diagnostics().live_source_groups != 0)
            && Instant::now() < final_deadline
        {
            thread::yield_now();
        }
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        assert_eq!(coordinator.diagnostics().live_source_groups, 0);
        drop(first);
        drop(second);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn coordinator_foreground_waits_for_a_preempted_session_permit_without_error() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let pool = MonitorSessionPool::new(1, 0);
        let coordinator = MonitorSourceCoordinator::new(1, pool.clone());
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let first_request = request(first_path.clone(), 1170);
        first.request(first_request.clone()).unwrap();
        let _ = receive_for(&first, &first_request);
        assert_eq!(pool.diagnostics().active_foreground_sessions, 1);

        let mut blocked = first_request;
        blocked.request_id = 1171;
        blocked.source_tick = 100_000;
        let barrier = install_test_decode_barrier(blocked.request_id, first_path.clone());
        first.request(blocked).unwrap();
        barrier.wait_until_blocked();
        assert!(first.defer_live_sessions().unwrap());

        let mut top = request(second_path.clone(), 1172);
        top.media_id = 4;
        second.request(top.clone()).unwrap();
        thread::sleep(Duration::from_millis(25));
        assert!(
            second.try_recv().unwrap().is_none(),
            "transient foreground permit pressure must not publish an error"
        );
        assert_eq!(pool.diagnostics().active_foreground_sessions, 1);

        barrier.release();
        let DecodeEvent::Frame(frame) = receive_for(&second, &top) else {
            panic!("top request did not recover after the preempted permit returned")
        };
        assert_eq!(frame.request_id, top.request_id);
        assert_frame_reaches_target(&frame, &top);

        first.cancel_pending().unwrap();
        second.release_live_sessions().unwrap();
        let final_deadline = Instant::now() + Duration::from_secs(2);
        while (pool.diagnostics().active_sticky_sessions != 0
            || coordinator.diagnostics().live_source_groups != 0
            || coordinator.diagnostics().retiring_lane_actors != 0)
            && Instant::now() < final_deadline
        {
            thread::yield_now();
        }
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        assert_eq!(coordinator.diagnostics().live_source_groups, 0);
        assert_eq!(coordinator.diagnostics().retiring_lane_actors, 0);
        drop(barrier);
        drop(first);
        drop(second);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn speculative_release_reclaims_background_before_foreground() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let pool = MonitorSessionPool::new(1, 1);
        let coordinator = MonitorSourceCoordinator::new(1, pool.clone());
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let mut prewarm = request(path.clone(), 1164);
        prewarm.prewarm_scrub_workers = true;
        decoder.request(prewarm).unwrap();
        assert_eq!(coordinator.diagnostics().live_source_groups, 1);
        assert_eq!(coordinator.diagnostics().live_lane_actors, 2);

        assert!(decoder.release_speculative_sessions().unwrap());
        let background_deadline = Instant::now() + Duration::from_secs(2);
        while (coordinator.diagnostics().live_lane_actors != 1
            || coordinator.diagnostics().retiring_lane_actors != 0)
            && Instant::now() < background_deadline
        {
            thread::yield_now();
        }
        let diagnostics = coordinator.diagnostics();
        assert_eq!(diagnostics.live_source_groups, 1);
        assert_eq!(diagnostics.live_lane_actors, 1);
        assert_eq!(diagnostics.retiring_lane_actors, 0);
        assert!(
            diagnostics.live_lane_actors + diagnostics.retiring_lane_actors
                <= diagnostics.lane_actor_cap
        );

        decoder.release_live_sessions().unwrap();
        let final_deadline = Instant::now() + Duration::from_secs(2);
        while (coordinator.diagnostics().live_source_groups != 0
            || coordinator.diagnostics().live_lane_actors != 0
            || coordinator.diagnostics().retiring_lane_actors != 0
            || pool.diagnostics().active_sticky_sessions != 0)
            && Instant::now() < final_deadline
        {
            thread::yield_now();
        }
        let final_diagnostics = coordinator.diagnostics();
        assert_eq!(final_diagnostics.live_source_groups, 0);
        assert_eq!(final_diagnostics.live_lane_actors, 0);
        assert_eq!(final_diagnostics.retiring_lane_actors, 0);
        assert_eq!(pool.diagnostics().active_sticky_sessions, 0);
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn speculative_release_preserves_visible_reverse_scrub_lane() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let pool = MonitorSessionPool::new(1, 1);
        let coordinator = MonitorSourceCoordinator::new(1, pool.clone());
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let mut forward = request(path.clone(), 1164);
        forward.is_scrubbing = true;
        forward.source_tick = 200_000;
        decoder.request(forward).unwrap();
        let mut reverse = request(path.clone(), 1165);
        reverse.is_scrubbing = true;
        reverse.source_tick = 100_000;
        decoder.request(reverse.clone()).unwrap();
        let event = receive_matching(&decoder, |frame| frame.request_id == reverse.request_id);
        assert!(matches!(event, DecodeEvent::Frame(_)));
        assert_eq!(coordinator.diagnostics().live_lane_actors, 2);
        let active_deadline = Instant::now() + Duration::from_secs(2);
        while decoder.diagnostics().active_sticky_sessions != 2 && Instant::now() < active_deadline
        {
            thread::yield_now();
        }
        assert_eq!(decoder.diagnostics().active_sticky_sessions, 2);
        assert_eq!(pool.diagnostics().active_sticky_sessions, 2);

        assert!(!decoder.release_speculative_sessions().unwrap());
        assert_eq!(coordinator.diagnostics().live_lane_actors, 2);
        assert_eq!(decoder.diagnostics().active_sticky_sessions, 2);
        assert_eq!(pool.diagnostics().active_sticky_sessions, 2);

        let mut further_reverse = request(path.clone(), 1166);
        further_reverse.is_scrubbing = true;
        further_reverse.source_tick = 50_000;
        decoder.request(further_reverse.clone()).unwrap();
        let event = receive_matching(&decoder, |frame| {
            frame.request_id == further_reverse.request_id
        });
        assert!(matches!(event, DecodeEvent::Frame(_)));
        let preserved_reverse_request_id = {
            let reverse_lease = decoder.workers[1]
                .endpoint
                .as_ref()
                .expect("coordinator endpoint")
                .active
                .lock()
                .expect("coordinator endpoint lease lock");
            let reverse_lease = reverse_lease.as_ref().expect("visible reverse lease");
            (reverse_lease.request.request_id, reverse_lease.speculative)
        };
        assert_eq!(preserved_reverse_request_id, (reverse.request_id, false));
        let continued_reverse_index = further_reverse.request_id as usize % decoder.workers.len();
        let continued_reverse_request_id = {
            let reverse_lease = decoder.workers[continued_reverse_index]
                .endpoint
                .as_ref()
                .expect("coordinator endpoint")
                .active
                .lock()
                .expect("coordinator endpoint lease lock");
            let reverse_lease = reverse_lease
                .as_ref()
                .expect("continued visible reverse lease");
            (reverse_lease.request.request_id, reverse_lease.speculative)
        };
        assert_eq!(
            continued_reverse_request_id,
            (further_reverse.request_id, false)
        );
        assert_eq!(decoder.diagnostics().active_sticky_sessions, 3);
        assert_eq!(pool.diagnostics().active_sticky_sessions, 2);

        decoder.release_live_sessions().unwrap();
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancel_clears_a_deferred_source_request_before_retry() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let coordinator = MonitorSourceCoordinator::new(1, MonitorSessionPool::new(1, 3));
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator,
        );
        let first_request = request(first_path.clone(), 1151);
        first.request(first_request.clone()).unwrap();
        let _ = receive_for(&first, &first_request);
        let mut deferred = request(second_path.clone(), 1152);
        deferred.media_id = 4;
        assert_eq!(
            second.request(deferred),
            Err(DecoderClosed::SourceCapacityDeferred)
        );
        second.cancel_pending().unwrap();
        first.release_live_sessions().unwrap();
        second.retry_deferred_requests().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(second.try_recv().unwrap().is_none());
        drop(first);
        drop(second);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn prewarm_preserves_source_capacity_deferred_outcome() {
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            MonitorSourceCoordinator::new(0, MonitorSessionPool::new(1, 3)),
        );
        let mut request = request(PathBuf::from("deferred-prewarm.mp4"), 1171);
        request.prewarm_scrub_workers = true;
        assert_eq!(
            decoder.request(request),
            Err(DecoderClosed::SourceCapacityDeferred)
        );
    }

    #[test]
    fn coordinator_background_lane_diagnostics_use_the_logical_worker_index() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let coordinator = MonitorSourceCoordinator::new(1, MonitorSessionPool::new(1, 3));
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            coordinator.clone(),
        );
        let mut forward = request(path.clone(), 1200);
        forward.is_scrubbing = true;
        forward.source_tick = 200_000;
        decoder.request(forward.clone()).unwrap();
        let _ = receive_for(&decoder, &forward);
        let mut reverse = forward.clone();
        reverse.request_id = 1203;
        reverse.source_tick = 0;
        decoder.request(reverse.clone()).unwrap();
        let _ = receive_for(&decoder, &reverse);
        assert_ne!(
            decoder
                .resource_diagnostics
                .active_sticky_session_mask
                .load(Ordering::Acquire)
                & (1 << 3),
            0
        );
        assert_eq!(coordinator.diagnostics().lane_actor_cap, 2);
        decoder.release_live_sessions().unwrap();
        assert_eq!(
            decoder
                .resource_diagnostics
                .active_sticky_session_mask
                .load(Ordering::Acquire),
            0
        );
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_actor_budget_bounds_same_source_lane_churn_while_retirement_is_blocked() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let pool = MonitorSessionPool::new(1, 3);
        let coordinator = MonitorSourceCoordinator::new(1, pool);
        let foreground =
            MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
                || {},
                MonitorFrameCachePool::new(1024 * 1024),
                coordinator.clone(),
            );
        let background =
            MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
                || {},
                MonitorFrameCachePool::new(1024 * 1024),
                coordinator.clone(),
            );

        let foreground_request = request(path.clone(), 1500);
        foreground.request(foreground_request.clone()).unwrap();
        let _ = receive_for(&foreground, &foreground_request);

        let blocked_request = request(path.clone(), 1503);
        let barrier = install_test_decode_barrier(blocked_request.request_id, path.clone());
        background
            .send_to(3, MonitorCommand::Request(blocked_request))
            .unwrap();
        barrier.wait_until_blocked();
        background.release_live_sessions().unwrap();

        let retiring_deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.diagnostics().retiring_lane_actors != 1
            && Instant::now() < retiring_deadline
        {
            thread::yield_now();
        }
        assert_eq!(coordinator.diagnostics().retiring_lane_actors, 1);

        let replacement = request(path.clone(), 1507);
        for _ in 0..32 {
            assert_eq!(
                background.send_to(3, MonitorCommand::Request(replacement.clone())),
                Err(DecoderClosed::SourceCapacityDeferred)
            );
            let diagnostics = coordinator.diagnostics();
            assert!(
                diagnostics.live_lane_actors + diagnostics.retiring_lane_actors
                    <= diagnostics.lane_actor_cap
            );
        }

        barrier.release();
        let reaped_deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.diagnostics().retiring_lane_actors != 0
            && Instant::now() < reaped_deadline
        {
            thread::yield_now();
        }
        assert_eq!(coordinator.diagnostics().retiring_lane_actors, 0);
        background.retry_deferred_requests().unwrap();
        let DecodeEvent::Frame(frame) = receive_for(&background, &replacement) else {
            panic!("deferred replacement did not decode after actor retirement")
        };
        assert_frame_reaches_target(&frame, &replacement);

        drop(background);
        drop(foreground);
        drop(barrier);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dead_client_queue_entries_are_pruned_before_the_bounded_queue_saturates() {
        let (wake, _wake_rx) = mpsc::sync_channel(1);
        let shared = Arc::new(SourceLaneActorShared {
            pending: Mutex::new(VecDeque::new()),
            queued_clients: Mutex::new(HashMap::new()),
            wake,
            shutdown: AtomicBool::new(false),
        });
        for id in 0..(MAX_SOURCE_ACTOR_CLIENTS * 2) as u64 {
            let client = Arc::new(CoordinatorClient {
                id,
                worker_index: 0,
                commands: Arc::new(Mutex::new(None)),
                endpoint: std::sync::Weak::new(),
            });
            assert!(enqueue_client(&shared, &client).is_ok());
        }
        prune_dead_clients(&shared);
        assert!(shared.queued_clients.lock().unwrap().len() < MAX_SOURCE_ACTOR_CLIENTS);
    }

    #[test]
    fn coordinator_cancel_invalidates_a_barrier_blocked_request_without_releasing_session() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let pool = MonitorSessionPool::new(1, 3);
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            MonitorSourceCoordinator::new(1, pool.clone()),
        );
        let request = request(path.clone(), 1201);
        let barrier = install_test_decode_barrier(request.request_id, path.clone());
        decoder.request(request).unwrap();
        barrier.wait_until_blocked();
        decoder.cancel_pending().unwrap();
        barrier.release();
        thread::sleep(Duration::from_millis(50));
        assert!(decoder.try_recv().unwrap().is_none());
        assert_eq!(pool.diagnostics().active_foreground_sessions, 1);
        decoder.release_live_sessions().unwrap();
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn coordinator_handoff_does_not_join_a_barrier_blocked_old_actor() {
        if !ffmpeg_available() {
            return;
        }
        let first_path = tiny_media();
        let second_path = tiny_media();
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            MonitorSourceCoordinator::new(1, MonitorSessionPool::new(2, 3)),
        );
        let first_request = request(first_path.clone(), 1301);
        let barrier = install_test_decode_barrier(first_request.request_id, first_path.clone());
        decoder.request(first_request).unwrap();
        barrier.wait_until_blocked();
        let mut second_request = request(second_path.clone(), 1302);
        second_request.media_id = 4;
        let started = Instant::now();
        decoder.request(second_request.clone()).unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        barrier.release();
        let DecodeEvent::Frame(frame) = receive_for(&decoder, &second_request) else {
            panic!("handoff request did not decode")
        };
        assert_frame_reaches_target(&frame, &second_request);
        drop(decoder);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn coordinator_republishes_a_newer_same_source_request_without_relocking_its_slot() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let decoder = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_source_coordinator(
            || {},
            MonitorFrameCachePool::new(1024 * 1024),
            MonitorSourceCoordinator::new(1, MonitorSessionPool::new(1, 3)),
        );
        let first = request(path.clone(), 1401);
        let barrier = install_test_decode_barrier(first.request_id, path.clone());
        decoder.request(first.clone()).unwrap();
        barrier.wait_until_blocked();

        let mut newer = first;
        newer.request_id = 1402;
        newer.source_tick = 100_000;
        decoder.request(newer.clone()).unwrap();
        barrier.release();

        let DecodeEvent::Frame(frame) = receive_for(&decoder, &newer) else {
            panic!("newer same-source request did not decode")
        };
        assert_frame_reaches_target(&frame, &newer);
        drop(decoder);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn shared_pool_bounds_multiple_decoders_and_releases_every_session_on_reset() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let pool = MonitorSessionPool::new(2, 1);
        let first = MonitorDecoder::new_with_notifier_and_cache_bytes_and_session_pool(
            || {},
            1024 * 1024,
            pool.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_cache_bytes_and_session_pool(
            || {},
            1024 * 1024,
            pool.clone(),
        );
        let mut prewarmed = request(path.clone(), 101);
        prewarmed.media_id = 11;
        prewarmed.prewarm_scrub_workers = true;
        let mut foreground = request(path.clone(), 202);
        foreground.media_id = 22;

        first.request(prewarmed.clone()).unwrap();
        second.request(foreground.clone()).unwrap();
        match receive_for(&first, &prewarmed) {
            DecodeEvent::Frame(frame) => {
                assert_eq!(frame.request_id, prewarmed.request_id);
                assert_eq!(frame.media_id, prewarmed.media_id);
                assert!(frame.source_tick >= prewarmed.source_tick);
            }
            DecodeEvent::Error(error) => panic!("prewarm decode failed: {}", error.message),
        }
        match receive_for(&second, &foreground) {
            DecodeEvent::Frame(frame) => {
                assert_eq!(frame.request_id, foreground.request_id);
                assert_eq!(frame.media_id, foreground.media_id);
                assert!(frame.source_tick >= foreground.source_tick);
            }
            DecodeEvent::Error(error) => panic!("foreground decode failed: {}", error.message),
        }
        for _ in 0..100 {
            if pool.diagnostics().peak_sticky_sessions == 3 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let saturated = pool.diagnostics();
        assert_eq!(saturated.session_cap, 3);
        assert_eq!(saturated.peak_sticky_sessions, 3);
        assert_eq!(saturated.active_foreground_sessions, 2);
        assert_eq!(saturated.active_background_sessions, 1);

        first.reset_live_cache().unwrap();
        second.reset_live_cache().unwrap();
        for _ in 0..100 {
            if pool.diagnostics().active_sticky_sessions == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let released = pool.diagnostics();
        assert_eq!(released.active_sticky_sessions, 0);
        assert_eq!(released.peak_sticky_sessions, 3);

        drop(first);
        drop(second);
        fs::remove_file(path).unwrap();
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
        let session_pool = MonitorSessionPool::new(1, 0);
        let session_permit = session_pool
            .try_acquire(MonitorSessionLane::Foreground)
            .expect("test monitor receives its foreground permit");
        Ok(StickyMonitor {
            _session_permit: session_permit,
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
        let session_pool = MonitorSessionPool::new(1, 0);

        let frame = recover_hardware_decode_failure(
            &mut sessions,
            &original,
            &commands,
            "injected D3D11VA runtime failure".to_owned(),
            &mut |_| {},
            &mut |_| {},
            &mut |_| {},
            &mut || {},
            &stage_timings,
            &session_pool,
            0,
            false,
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
        let session_pool = MonitorSessionPool::new(1, 0);

        let event = decode_monitor_request(
            &mut sessions,
            &cache,
            &desired,
            &commands,
            &mut |_| {},
            &mut |active| session_states.push(active),
            &mut || {},
            &stage_timings,
            &session_pool,
            0,
            false,
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
    fn shared_frame_cache_pool_reuses_pixels_without_rewriting_event_identity() {
        let pool = MonitorFrameCachePool::new(8 * 1024);
        let session_pool = MonitorSessionPool::new(2, 2);
        let first = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_session_pool(
            || {},
            pool.clone(),
            session_pool.clone(),
        );
        let second = MonitorDecoder::new_with_notifier_and_frame_cache_pool_and_session_pool(
            || {},
            pool.clone(),
            session_pool,
        );
        let first_request = request(PathBuf::from("shared-cache-must-not-open.mp4"), 101);
        let mut second_request = first_request.clone();
        second_request.request_id = 202;
        second_request.project_epoch = 99;
        {
            let mut cache = pool.cache.lock().expect("shared frame cache lock");
            cache.prepare_request(&first_request);
            assert!(cache.insert_if_current(
                &first_request,
                frame_cache_key(&first_request, first_request.source_tick),
                FrameValue::new(0, 40, 30, vec![7; 40 * 30 * 4].into()),
            ));
        }

        first.request(first_request.clone()).unwrap();
        second.request(second_request.clone()).unwrap();
        let DecodeEvent::Frame(first_frame) = receive_for(&first, &first_request) else {
            panic!("first shared-cache response was not a frame");
        };
        let DecodeEvent::Frame(second_frame) = receive_for(&second, &second_request) else {
            panic!("second shared-cache response was not a frame");
        };
        assert_eq!(first_frame.request_id, first_request.request_id);
        assert_eq!(first_frame.project_epoch, first_request.project_epoch);
        assert_eq!(second_frame.request_id, second_request.request_id);
        assert_eq!(second_frame.project_epoch, second_request.project_epoch);
        assert!(Arc::ptr_eq(&first_frame.rgba, &second_frame.rgba));
        let cache = pool.diagnostics();
        assert_eq!(cache.capacity_bytes, 8 * 1024);
        assert_eq!(cache.current_bytes, 40 * 30 * 4);
        assert_eq!(
            first.diagnostics().frame_cache_capacity_bytes,
            cache.capacity_bytes
        );
        assert_eq!(
            second.diagnostics().frame_cache_capacity_bytes,
            cache.capacity_bytes
        );
    }

    #[test]
    fn shared_frame_cache_pool_does_not_alias_source_or_scaler_policy() {
        let pool = MonitorFrameCachePool::new(8 * 1024);
        let original = request(PathBuf::from("shared-cache-source-a.mp4"), 1);
        let key = frame_cache_key(&original, original.source_tick);
        let mut cache = pool.cache.lock().expect("shared frame cache lock");
        cache.prepare_request(&original);
        assert!(cache.insert_if_current(
            &original,
            key,
            FrameValue::new(0, 40, 30, vec![9; 40 * 30 * 4].into()),
        ));

        let mut distinct_source = original.clone();
        distinct_source.media_id = 4;
        distinct_source.path = PathBuf::from("shared-cache-source-b.mp4");
        cache.prepare_request(&distinct_source);
        assert!(cache.get(&frame_cache_key(&distinct_source, 0)).is_none());

        cache.prepare_request(&original);
        assert!(cache.insert_if_current(
            &original,
            key,
            FrameValue::new(0, 40, 30, vec![8; 40 * 30 * 4].into()),
        ));
        let mut relinked_source = original.clone();
        relinked_source.path = PathBuf::from("shared-cache-source-relinked.mp4");
        assert!(cache.prepare_request(&relinked_source));
        assert!(cache.get(&key).is_none());

        cache.prepare_request(&original);
        assert!(cache.insert_if_current(
            &original,
            key,
            FrameValue::new(0, 40, 30, vec![7; 40 * 30 * 4].into()),
        ));
        let mut distinct_output = original.clone();
        distinct_output.width = 80;
        assert!(
            cache
                .get(&frame_cache_key(
                    &distinct_output,
                    distinct_output.source_tick
                ))
                .is_none()
        );

        let mut distinct_epoch = original.clone();
        distinct_epoch.cache_epoch += 1;
        assert!(cache.prepare_request(&distinct_epoch));
        assert!(
            cache
                .get(&frame_cache_key(
                    &distinct_epoch,
                    distinct_epoch.source_tick
                ))
                .is_none()
        );

        let mut distinct_policy = original.clone();
        distinct_policy.high_quality_scaling = false;
        assert!(cache.prepare_request(&distinct_policy));
        assert!(cache.get(&frame_cache_key(&distinct_policy, 0)).is_none());
    }

    #[test]
    fn unavailable_background_session_defers_without_publishing_decode_error() {
        let desired = request(
            PathBuf::from("must-not-open-while-background-is-full.mp4"),
            92,
        );
        let mut sessions = HashMap::new();
        let cache = Arc::new(Mutex::new(MonitorFrameCache::new(0)));
        let commands = Arc::new(Mutex::new(None));
        let stage_timings = DecoderStageTimingAccumulators::default();
        let session_pool = MonitorSessionPool::new(1, 0);
        let mut deferred = false;

        let event = decode_monitor_request(
            &mut sessions,
            &cache,
            &desired,
            &commands,
            &mut |_| {},
            &mut |_| {},
            &mut || deferred = true,
            &stage_timings,
            &session_pool,
            1,
            false,
        );

        assert!(event.is_none());
        assert!(deferred);
        assert!(sessions.is_empty());
        assert_eq!(session_pool.diagnostics().active_sticky_sessions, 0);
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
        let diagnostics = Arc::new(FrameCacheDiagnostics::new(32));
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
        assert_eq!(snapshot.capacity_bytes, 32);
        assert_eq!(snapshot.current_bytes, 32);
        assert_eq!(snapshot.peak_bytes, 32);
        assert_eq!(snapshot.eviction_count, 1);
        assert!(snapshot.current_bytes <= snapshot.capacity_bytes);

        cache.clear();
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.current_bytes, 0);
        assert_eq!(snapshot.peak_bytes, 32);
        assert_eq!(snapshot.eviction_count, 1);
    }

    #[test]
    fn frame_cache_pool_diagnostics_publish_cumulative_budget_evictions() {
        let pool = MonitorFrameCachePool::new(32);
        let key = FrameKey {
            project_epoch: 1,
            media_id: 2,
            source_tick: 0,
            width: 2,
            height: 2,
        };
        let mut cache = pool.cache.lock().expect("frame cache pool lock");
        for source_tick in [
            0,
            SPARSE_CACHE_INTERVAL_TICKS,
            SPARSE_CACHE_INTERVAL_TICKS * 2,
        ] {
            let frame_key = FrameKey { source_tick, ..key };
            assert!(cache.insert(
                frame_key,
                FrameValue::new(source_tick, 2, 2, vec![source_tick as u8; 16].into()),
            ));
        }
        drop(cache);

        let snapshot = pool.diagnostics();
        assert_eq!(snapshot.capacity_bytes, 32);
        assert_eq!(snapshot.current_bytes, 32);
        assert_eq!(snapshot.peak_bytes, 32);
        assert_eq!(snapshot.eviction_count, 1);

        pool.cache.lock().expect("frame cache pool lock").clear();
        let snapshot = pool.diagnostics();
        assert_eq!(snapshot.current_bytes, 0);
        assert_eq!(snapshot.peak_bytes, 32);
        assert_eq!(snapshot.eviction_count, 1);
    }

    #[test]
    fn worker_session_diagnostics_aggregate_with_fixed_cap() {
        let diagnostics = DecoderResourceDiagnostics::new();
        diagnostics.publish_worker_session(0, true);
        diagnostics.publish_worker_session(2, true);
        let snapshot = diagnostics.snapshot(MonitorFrameCachePoolDiagnostics::default());
        assert_eq!(snapshot.active_sticky_sessions, 2);
        assert_eq!(snapshot.peak_sticky_sessions, 2);
        assert_eq!(snapshot.session_cap, MONITOR_WORKER_COUNT);

        diagnostics.publish_worker_session(0, false);
        diagnostics.publish_worker_session(2, false);
        let snapshot = diagnostics.snapshot(MonitorFrameCachePoolDiagnostics::default());
        assert_eq!(snapshot.active_sticky_sessions, 0);
        assert_eq!(snapshot.peak_sticky_sessions, 2);
        assert!(snapshot.peak_sticky_sessions <= snapshot.session_cap);
    }

    #[test]
    fn diagnostics_snapshots_remain_peak_coherent_while_publishing() {
        let diagnostics = Arc::new(DecoderResourceDiagnostics::new());
        let cache_diagnostics = Arc::new(FrameCacheDiagnostics::new(128));
        let publishing = Arc::new(AtomicBool::new(true));
        let writer_diagnostics = Arc::clone(&diagnostics);
        let writer_cache_diagnostics = Arc::clone(&cache_diagnostics);
        let writer_publishing = Arc::clone(&publishing);
        let writer = thread::spawn(move || {
            for _ in 0..10_000 {
                writer_cache_diagnostics.publish(128, 1);
                writer_diagnostics.publish_worker_session(0, true);
                writer_cache_diagnostics.publish(0, 1);
                writer_diagnostics.publish_worker_session(0, false);
            }
            writer_publishing.store(false, Ordering::Release);
        });

        while publishing.load(Ordering::Acquire) {
            let snapshot = diagnostics.snapshot(cache_diagnostics.snapshot());
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

        assert!(
            cache.get_scrub_at_or_after(&desired).is_none(),
            "unknown timing must not substitute a future frame from an invented CFR window"
        );
        desired.source_tick = 1_000_000;
        assert_eq!(
            cache
                .get_scrub_at_or_after(&desired)
                .expect("an exact unknown-timing cache match remains reusable")
                .source_tick,
            1_000_000
        );
        desired.source_frame_duration_tick = Some(33_334);
        desired.source_tick = 950_000;
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
                .expect("known high-rate cache lookup stays within one source frame")
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
    fn blocked_decoder_request_does_not_prevent_another_decoder_from_completing() {
        if !ffmpeg_available() {
            return;
        }
        let path = tiny_media();
        let (first_ready_tx, first_ready_rx) = mpsc::channel();
        let first = MonitorDecoder::new_with_notifier(move || {
            let _ = first_ready_tx.send(());
        });
        let (second_ready_tx, second_ready_rx) = mpsc::channel();
        let second = MonitorDecoder::new_with_notifier(move || {
            let _ = second_ready_tx.send(());
        });
        let first_request = request(path.clone(), 101);
        let second_request = request(path.clone(), 102);
        // The guard is declared after both workers and requests, so every unwind releases the
        // blocked worker before either decoder's Drop joins its scheduler.
        let barrier = install_test_decode_barrier(first_request.request_id, path.clone());

        first.request(first_request.clone()).unwrap();
        barrier.wait_until_blocked();
        second.request(second_request.clone()).unwrap();
        second_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("independent decoder did not complete while the first was blocked");
        match second.try_recv().unwrap().expect("second decoder event") {
            DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &second_request),
            DecodeEvent::Error(error) => {
                panic!("unexpected independent decode error: {}", error.message)
            }
        }

        barrier.release();
        first_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("blocked decoder did not resume after release");
        match first.try_recv().unwrap().expect("first decoder event") {
            DecodeEvent::Frame(frame) => assert_frame_reaches_target(&frame, &first_request),
            DecodeEvent::Error(error) => {
                panic!("unexpected resumed decode error: {}", error.message)
            }
        }
        drop(first);
        drop(second);
        drop(barrier);
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
        let session_pool = MonitorSessionPool::new(1, 0);
        let mut monitor = open_sticky_monitor(&first, &session_pool, 0, false)
            .expect("open sticky monitor")
            .expect("foreground permit available");
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
        let session_pool = MonitorSessionPool::new(1, 3);
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
                &mut || {},
                &stage_timings,
                &session_pool,
                0,
                false,
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
        let session_pool = MonitorSessionPool::new(1, 3);
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
            &mut || {},
            &stage_timings,
            &session_pool,
            0,
            false,
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
        let session_pool = MonitorSessionPool::new(1, 0);
        let mut monitor = open_sticky_monitor(&first, &session_pool, 0, false)
            .expect("open hardware monitor")
            .expect("foreground permit available");
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
