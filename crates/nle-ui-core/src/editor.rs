//! The editor shell. Media data enters this crate only after the app layer has chosen it.

use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use egui::{
    Align, Color32, FontId, Frame, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    Vec2,
};
#[cfg(test)]
use nle_compositor::video_fade_opacity;
use nle_compositor::{
    CompositeLayerInput, CompositeQuad, CompositionRequest, MAX_COMPOSITE_LAYERS, PixelSize,
    fade_envelope_value, plan_composition, video_opacity_at,
};
use nle_timeline::{
    AnimatedScalar, AudioEffect, AudioTransitionId, BrightnessContrastEffect, Clip, ClipId,
    ColorCurve, ColorParameter, CurvePoint, EditTarget, EvaluatedVideoEffectStack, Fade, FadeEdge,
    KeyframeInterpolation, MAX_AUDIO_EFFECTS_PER_SCOPE, MAX_BLACKS, MAX_BRIGHTNESS,
    MAX_COLOR_CURVE_POINTS, MAX_CONTRAST, MAX_EXPOSURE, MAX_GAIN_DB, MAX_HIGHLIGHTS, MAX_PAN,
    MAX_SATURATION, MAX_SHADOWS, MAX_TEMPERATURE, MAX_TINT, MAX_VIDEO_EFFECTS_PER_CLIP,
    MAX_VIGNETTE_AMOUNT, MAX_VIGNETTE_CENTER, MAX_VIGNETTE_FEATHER, MAX_VIGNETTE_MIDPOINT,
    MAX_WHITES, MIN_BLACKS, MIN_BRIGHTNESS, MIN_CONTRAST, MIN_EXPOSURE, MIN_GAIN_DB,
    MIN_HIGHLIGHTS, MIN_PAN, MIN_SATURATION, MIN_SHADOWS, MIN_TEMPERATURE, MIN_TINT,
    MIN_VIGNETTE_AMOUNT, MIN_VIGNETTE_CENTER, MIN_VIGNETTE_FEATHER, MIN_VIGNETTE_MIDPOINT,
    MIN_WHITES, MediaId as TimelineMediaId, Tick, Timeline, TimelineCache, TimelineError,
    TimelineSnapshot, TimelineSnapshotError, TitleAlignment, TitleColor, TitleId, TitleOverlay,
    Track, TrackDrawRecord, TrackId, TrackKind, TransitionId, UndoStack, VideoEffectId,
    VideoEffectKind, VideoEffectNode, VideoTransition, VideoTransitionKind, VignetteEffect,
};
use serde::{Deserialize, Serialize};

use crate::Language;

pub type MediaId = u32;

pub const EDITOR_PROJECT_SNAPSHOT_VERSION: u32 = 1;
/// The viewer composites this many visible video tracks without delaying the UI thread.
///
/// This is intentionally bounded: each slot owns independent decode, retained-frame, and
/// invalidation state so a late layer never blocks the other three.
pub const PREVIEW_VIDEO_LAYER_COUNT: usize = 4;
/// Default timeline span for a still image before a user trims or extends it.
pub const DEFAULT_STILL_IMAGE_DURATION: Tick = Tick(5_000_000);
const DEFAULT_VIDEO_TRANSITION_DURATION: Tick = Tick(1_000_000);
const PROVISIONAL_MEDIA_DURATION: Tick = Tick(15_000_000);
const LEGACY_DEFAULT_TIMELINE_HEIGHT: f32 = 330.0;
const PREVIOUS_DEFAULT_TIMELINE_HEIGHT: f32 = 520.0;
const FORMER_DEFAULT_TIMELINE_HEIGHT: f32 = 640.0;
const PREVIOUS_LAYOUT_DEFAULT_TIMELINE_HEIGHT: f32 = 760.0;
const DEFAULT_TIMELINE_HEIGHT: f32 = 440.0;
const DEFAULT_TIMELINE_HEIGHT_FRACTION: f32 = 0.38;
const DEFAULT_MEDIA_POOL_WIDTH: f32 = 420.0;
const DEFAULT_RIGHT_SIDEBAR_WIDTH: f32 = 450.0;
const EDITOR_OUTER_INSET: i8 = 6;
const VIDEO_TRANSITION_KINDS: [VideoTransitionKind; 12] = [
    VideoTransitionKind::CrossDissolve,
    VideoTransitionKind::FilmDissolve,
    VideoTransitionKind::DipToBlack,
    VideoTransitionKind::DipToWhite,
    VideoTransitionKind::WipeLeft,
    VideoTransitionKind::WipeRight,
    VideoTransitionKind::WipeUp,
    VideoTransitionKind::WipeDown,
    VideoTransitionKind::SlideFromLeft,
    VideoTransitionKind::SlideFromRight,
    VideoTransitionKind::SlideFromTop,
    VideoTransitionKind::SlideFromBottom,
];
/// Title, tools, navigator, zoom, status, and a useful minimum track viewport.
const MIN_COMPLETE_TIMELINE_PANEL_HEIGHT: f32 = 336.0;

/// GPU-neutral timeline layer.
///
/// The application owns any native renderer and installs its backing paint operation from
/// [`TimelineCanvas::begin`].  UI-core subsequently sends only axis-aligned primitives to that
/// operation. Solid geometry and thumbnail atlas cells are sent to that operation. Text, curve
/// geometry, and non-timeline thumbnails intentionally remain egui overlays so input tests and
/// the non-GPU fallback stay deterministic.
pub trait TimelineCanvas {
    /// Starts one timeline paint operation. Implementations that use an egui paint callback
    /// must install it here, before UI-core emits overlay shapes.
    fn begin(&mut self, ui: &mut Ui, canvas_rect: Rect);

    /// Adds one opaque or alpha-blended axis-aligned fill in screen coordinates.
    fn solid_rect(&mut self, rect: Rect, color: Color32);

    /// Adds a textured atlas cell in screen coordinates.
    ///
    /// `native_texture_id` is an application-owned key for the native timeline renderer.
    /// `fallback_texture` keeps the headless/egui implementation functional without exposing
    /// renderer types to UI-core.
    fn texture_rect(
        &mut self,
        rect: Rect,
        native_texture_id: u64,
        fallback_texture: egui::TextureId,
        uv: Rect,
        tint: Color32,
    );
}

/// GPU-neutral viewer layer submitted after UI-core resolves shared compositor geometry.
///
/// Native applications can retain and composite decoded textures without exposing renderer
/// ownership to UI-core. Headless tests and fallback callers continue to paint egui meshes.
pub trait ViewerCanvas {
    /// Starts one viewer frame and installs any native paint callback behind viewer overlays.
    fn begin(&mut self, ui: &mut Ui, canvas_rect: Rect, project_size: PixelSize);

    /// Submits one ready layer in bottom-to-top order.
    fn layer(
        &mut self,
        layer: usize,
        frame: MonitorFrame,
        content_uv: Rect,
        quad: CompositeQuad,
        effects: EvaluatedVideoEffectStack,
    );

    /// Draws project black at the current layer boundary without consuming a decoder slot.
    fn black_matte(&mut self, opacity: f32);

    /// Draws project white at the current layer boundary. Native canvases that have not yet
    /// supplied a color-matte primitive keep the existing safe black fallback; the egui canvas
    /// implements the white matte directly.
    fn white_matte(&mut self, opacity: f32) {
        self.black_matte(opacity);
    }
}

/// Test, headless, and device-loss fallback for the professional viewer canvas.
pub struct EguiViewerCanvas {
    painter: Option<egui::Painter>,
    canvas: Rect,
    project_size: Option<PixelSize>,
}

impl Default for EguiViewerCanvas {
    fn default() -> Self {
        Self {
            painter: None,
            canvas: Rect::NOTHING,
            project_size: None,
        }
    }
}

impl ViewerCanvas for EguiViewerCanvas {
    fn begin(&mut self, ui: &mut Ui, canvas_rect: Rect, project_size: PixelSize) {
        self.painter = Some(ui.painter().with_clip_rect(canvas_rect));
        self.canvas = canvas_rect;
        self.project_size = Some(project_size);
    }

    fn layer(
        &mut self,
        _layer: usize,
        frame: MonitorFrame,
        content_uv: Rect,
        quad: CompositeQuad,
        _effects: EvaluatedVideoEffectStack,
    ) {
        let (Some(painter), Some(project_size)) = (&self.painter, self.project_size) else {
            return;
        };
        paint_composite_quad(painter, self.canvas, project_size, frame, content_uv, quad);
    }

    fn black_matte(&mut self, opacity: f32) {
        let Some(painter) = &self.painter else {
            return;
        };
        painter.rect_filled(
            self.canvas,
            0.0,
            Color32::from_black_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8),
        );
    }

    fn white_matte(&mut self, opacity: f32) {
        let Some(painter) = &self.painter else {
            return;
        };
        painter.rect_filled(
            self.canvas,
            0.0,
            Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8),
        );
    }
}

/// Test and headless fallback that uses egui's painter directly.
#[derive(Default)]
pub struct EguiTimelineCanvas {
    painter: Option<egui::Painter>,
}

impl TimelineCanvas for EguiTimelineCanvas {
    fn begin(&mut self, ui: &mut Ui, canvas_rect: Rect) {
        self.painter = Some(ui.painter().with_clip_rect(canvas_rect));
    }

    fn solid_rect(&mut self, rect: Rect, color: Color32) {
        if let Some(painter) = &self.painter {
            painter.rect_filled(rect, 0.0, color);
        }
    }

    fn texture_rect(
        &mut self,
        rect: Rect,
        _native_texture_id: u64,
        fallback_texture: egui::TextureId,
        uv: Rect,
        tint: Color32,
    ) {
        if let Some(painter) = &self.painter {
            painter.image(fallback_texture, rect, uv, tint);
        }
    }
}

/// Durable, versioned editor state. Runtime decoding and GPU state deliberately do not cross
/// this boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditorProjectSnapshot {
    pub version: u32,
    pub media: Vec<EditorMediaSnapshot>,
    pub timeline: TimelineSnapshot,
    pub view: EditorViewSnapshot,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditorMediaSnapshot {
    pub id: MediaId,
    pub path: PathBuf,
    /// Worker-probed source duration. Runtime waveform and thumbnail caches remain excluded.
    #[serde(default)]
    pub duration: Option<Tick>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditorViewSnapshot {
    pub playhead: Tick,
    pub selected_media: Option<MediaId>,
    pub selected_timeline_clip: Option<ClipId>,
    #[serde(default)]
    pub selected_title: Option<TitleId>,
    pub zoom_left: f32,
    pub zoom_right: f32,
    pub media_pool_width: f32,
    pub analysis_width: f32,
    #[serde(default = "default_undertow_tools_width")]
    pub undertow_tools_width: f32,
    #[serde(default = "default_undertow_mixer_width")]
    pub undertow_mixer_width: f32,
    pub timeline_height: f32,
    #[serde(default)]
    pub timeline_height_is_default: Option<bool>,
    pub timeline_scroll_y: f32,
    pub track_heights: Vec<TrackHeightSnapshot>,
    #[serde(default)]
    pub timeline_view_start: Tick,
    #[serde(default)]
    pub timeline_view_span: Tick,
    #[serde(default = "default_true")]
    pub snapping: bool,
    #[serde(default = "default_linked_selection")]
    pub linked_selection: bool,
    #[serde(default)]
    pub position_lock: bool,
    #[serde(default = "default_true")]
    pub show_video_thumbnails: bool,
    #[serde(default = "default_true")]
    pub show_audio_waveforms: bool,
    /// User-selected moving playback and scrub decode quality. Automatic adaptation remains
    /// runtime-only.
    #[serde(default)]
    pub preview_quality: PreviewQuality,
    /// Distinguishes a deliberate Auto choice from projects saved when Auto was the default.
    #[serde(default)]
    pub preview_quality_is_explicit: bool,
    /// User-selected resolution used when playback is paused.
    #[serde(default)]
    pub paused_preview_quality: PreviewQuality,
    /// Keep the normal playback pipeline at its full-quality path unless the user opts out.
    #[serde(default = "default_true")]
    pub high_quality_playback: bool,
    #[serde(default)]
    pub track_density: TimelineTrackDensity,
    #[serde(default)]
    pub markers: Vec<TimelineMarker>,
    #[serde(default)]
    pub flags: Vec<TimelineFlag>,
    /// Clips whose initial duration is still owned by the background media probe.
    #[serde(default)]
    pub provisional_clip_ids: Option<Vec<ClipId>>,
    /// Retains automatic fit ownership while unresolved placeholder clips remain.
    #[serde(default)]
    pub auto_fit_provisional_view: Option<bool>,
}

/// Decode resolution used by the live viewer. This never changes export resolution.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub enum PreviewQuality {
    /// Uses the application's runtime performance policy. This is opt-in.
    Auto,
    #[default]
    Full,
    Half,
    Quarter,
    Eighth,
}

impl PreviewQuality {
    /// The integer divisor applied to the quantized viewer size.
    ///
    /// `Auto` returns one because its effective scale is exposed separately through
    /// [`EditorState::resolved_preview_quality`].
    pub const fn divisor(self) -> u32 {
        match self {
            Self::Auto | Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
            Self::Eighth => 8,
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_linked_selection() -> bool {
    true
}
fn default_undertow_tools_width() -> f32 {
    190.0
}
fn default_undertow_mixer_width() -> f32 {
    220.0
}

/// Compatibility mapping for projects saved before timeline panning was durable.
fn legacy_zoom_span(left: f32, right: f32) -> Tick {
    let left = finite_or(left, 0.08);
    let right = finite_or(right, 0.92);
    Tick((300_000_000.0 * (right - left).clamp(0.01, 1.0)).round() as i64)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum TimelineTrackDensity {
    Compact,
    #[default]
    Normal,
    Large,
}

/// A color-palette index is durable, while the UI owns the actual display color.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimelineMarker {
    pub tick: Tick,
    pub color: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineFlag {
    /// Resolve-style flags belong to source media, so every timeline instance displays them.
    pub media_id: MediaId,
    pub color: u8,
    /// Only populated while reading pre-source-flag snapshots; stripped on the next save.
    legacy_clip_id: Option<ClipId>,
}

impl Serialize for TimelineFlag {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut output = serializer.serialize_struct("TimelineFlag", 2)?;
        output.serialize_field("media_id", &self.media_id)?;
        output.serialize_field("color", &self.color)?;
        output.end()
    }
}

impl<'de> Deserialize<'de> for TimelineFlag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            media_id: Option<MediaId>,
            #[serde(default)]
            clip_id: Option<ClipId>,
            color: u8,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.media_id.is_none() && wire.clip_id.is_none() {
            return Err(serde::de::Error::missing_field("media_id"));
        }
        Ok(Self {
            media_id: wire.media_id.unwrap_or(0),
            color: wire.color,
            legacy_clip_id: wire.clip_id,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrackHeightSnapshot {
    pub track_id: TrackId,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorRestoreError {
    UnsupportedVersion(u32),
    InvalidMediaId(MediaId),
    DuplicateMediaId(MediaId),
    NonContiguousMediaId { expected: MediaId, actual: MediaId },
    DuplicateMediaPath(PathBuf),
    UnknownTimelineMedia(MediaId),
    InvalidTimeline(TimelineSnapshotError),
}

impl fmt::Display for EditorRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid editor project snapshot: {self:?}")
    }
}

impl std::error::Error for EditorRestoreError {}

/// The clip selected for monitor decoding. The app layer owns all decoder work;
/// this is simply the source address at the current playhead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackTarget<'a> {
    pub clip_id: ClipId,
    pub media_id: MediaId,
    pub path: &'a Path,
    pub source_tick: Tick,
    /// The source address to decode. Still images keep this fixed while their
    /// logical `source_tick` continues advancing for animated effects.
    pub decode_tick: Tick,
    /// Exact positive source frame rate used to coalesce monitor requests on source-frame
    /// boundaries without rounding NTSC-style rates.
    pub source_frame_rate: Option<SourceFrameRate>,
    /// Duration to the following indexed source-frame boundary, when packet timing supplied it.
    pub source_frame_duration_tick: Option<Tick>,
    /// Clip-local video opacity after applying the default fade-to-black envelope.
    pub opacity: f32,
    /// Opaque project-black matte inserted immediately before this decoded layer.
    pub black_matte_before: f32,
    /// Opaque project-black matte inserted immediately after this decoded layer.
    pub black_matte_after: f32,
    /// Opaque project-white matte inserted immediately before this decoded layer.
    pub white_matte_before: f32,
    /// Opaque project-white matte inserted immediately after this decoded layer.
    pub white_matte_after: f32,
    /// Runtime-only geometric reveal applied after the clip transform.
    pub transition_reveal: Option<TransitionReveal>,
    /// Runtime-only project-space slide offset applied after the clip transform.
    pub transition_offset: (f32, f32),
    /// Native media dimensions when the probe supplied a usable video size.  The decoder can
    /// still supply a safe fallback when metadata is unavailable.
    pub source_size: Option<(u32, u32)>,
    pub transform: nle_timeline::ClipTransform,
    /// Source-time evaluated color stack. It is composition state, not a decode key.
    pub video_effects: EvaluatedVideoEffectStack,
}

/// An exact positive source frame rate supplied by media probing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFrameRate {
    numerator: u64,
    denominator: u64,
}

impl SourceFrameRate {
    pub fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if numerator == 0 || denominator == 0 {
            return None;
        }
        let divisor = gcd_u64(numerator, denominator);
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

/// Maximum retained source-frame boundaries for one runtime media index.
/// One million microsecond ticks occupy roughly 8 MiB and bound background-analysis output.
pub const MAX_SOURCE_FRAME_TIME_INDEX_POINTS: usize = 1_000_000;

/// Runtime-only packet-derived source-frame boundaries for one media item.
///
/// This deliberately stays out of snapshots: it can be rebuilt from the source file and must
/// never make a project dirty merely because media analysis completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFrameTimeIndex {
    ticks: Vec<Tick>,
}

impl SourceFrameTimeIndex {
    /// Accepts only complete, nonnegative, strictly increasing timing data within the fixed cap.
    pub fn new(ticks: Vec<Tick>) -> Option<Self> {
        (ticks.len() <= MAX_SOURCE_FRAME_TIME_INDEX_POINTS
            && ticks.iter().all(|tick| tick.0 >= 0)
            && ticks.windows(2).all(|pair| pair[0] < pair[1]))
        .then_some(Self { ticks })
    }

    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    pub fn ticks(&self) -> &[Tick] {
        &self.ticks
    }

    pub fn into_ticks(self) -> Vec<Tick> {
        self.ticks
    }

    fn resolve(&self, logical_source_tick: Tick) -> Option<(Tick, Option<Tick>)> {
        let index = self
            .ticks
            .partition_point(|tick| *tick <= logical_source_tick)
            .saturating_sub(1);
        let tick = *self.ticks.get(index)?;
        let duration = self.ticks.get(index + 1).map(|next| Tick(next.0 - tick.0));
        Some((tick, duration))
    }
}

fn gcd_u64(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

/// The direction an incoming transition layer is revealed in monitor space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// These public directional names are used throughout transition rendering; retain them to avoid
// changing the established transition-facing API.
#[allow(clippy::enum_variant_names)]
pub enum TransitionReveal {
    FromLeft,
    FromRight,
    FromTop,
    FromBottom,
}

/// An audible clip at the playhead. The app owns native device I/O; this is
/// deliberately only timeline state and a media path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioPlaybackTransitionRole {
    Outgoing,
    Incoming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioPlaybackTransitionEnvelope {
    pub role: AudioPlaybackTransitionRole,
    pub start_clip_tick: Tick,
    pub duration_ticks: Tick,
}

pub struct AudioPlaybackTarget<'a> {
    pub track_id: nle_timeline::TrackId,
    pub clip_id: ClipId,
    pub media_id: MediaId,
    pub path: &'a Path,
    pub source_tick: Tick,
    pub clip_tick: Tick,
    pub gain_db: f32,
    pub gain_left_db: f32,
    pub gain_right_db: f32,
    pub pan: f32,
    /// Enabled clip processors followed by enabled track processors in audible stack order.
    pub effects: Vec<AudioEffect>,
    pub fade_in_ticks: Tick,
    pub fade_in_curve: f32,
    pub fade_out_ticks: Tick,
    pub fade_out_curve: f32,
    pub clip_duration: Tick,
    pub transition: Option<AudioPlaybackTransitionEnvelope>,
}

/// Allocation-free source metadata for scheduling snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioPlaybackSource {
    pub track_id: nle_timeline::TrackId,
    pub clip_id: ClipId,
    pub media_id: MediaId,
    pub source_tick: Tick,
    pub clip_tick: Tick,
    pub transition: Option<AudioPlaybackTransitionEnvelope>,
}

struct ResolvedAudioPlaybackSource<'a> {
    track: &'a Track,
    clip: &'a Clip,
    path: &'a Path,
    source: AudioPlaybackSource,
}

fn enabled_audio_effects(clip: &Clip, track: &Track) -> Vec<AudioEffect> {
    clip.effects
        .iter()
        .chain(&track.effects)
        .filter(|effect| !matches!(effect, AudioEffect::Bypassed(_)))
        .cloned()
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorStatus {
    Empty,
    Ready,
    Error(String),
}

/// GPU texture supplied by the application after background FFmpeg decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorFrame {
    pub texture: egui::TextureId,
    pub width: u32,
    pub height: u32,
    /// Identifies the source of the latest live-decoded frame.
    pub media_id: Option<MediaId>,
    pub source_tick: Option<Tick>,
}

/// Origin of the image currently contributing to a live preview layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePreviewSourceKind {
    OriginalSource,
    InternalScrubPreview,
}

/// Decoder backend actually used for a live preview layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePreviewDecoderBackend {
    Software,
    IntelQuickSync,
    NvidiaCuvid,
    AppleVideoToolbox,
    WindowsD3d11va,
    WindowsDxva2,
}

/// Why a live preview layer fell back from the requested hardware path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePreviewFallbackReason {
    ForcedSoftware,
    HardwareUnavailable,
    HardwareDecodeFailed,
}

/// Allocation-free runtime evidence for one active preview layer.
///
/// This is supplied by the application after it selects and resolves a decode path. It is never
/// serialized into a project snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivePreviewDiagnostic {
    pub media_id: MediaId,
    pub source_kind: ActivePreviewSourceKind,
    /// `None` means the UI has not observed a concrete decoder for this layer yet.
    pub decoder_backend: Option<ActivePreviewDecoderBackend>,
    pub fallback_reason: Option<ActivePreviewFallbackReason>,
    pub selected_quality: PreviewQuality,
    pub resolved_quality: PreviewQuality,
    pub width: u32,
    pub height: u32,
}

impl ActivePreviewDiagnostic {
    pub const fn new(
        media_id: MediaId,
        source_kind: ActivePreviewSourceKind,
        decoder_backend: Option<ActivePreviewDecoderBackend>,
        fallback_reason: Option<ActivePreviewFallbackReason>,
        selected_quality: PreviewQuality,
        resolved_quality: PreviewQuality,
        dimensions: [u32; 2],
    ) -> Self {
        let [width, height] = dimensions;
        Self {
            media_id,
            source_kind,
            decoder_backend,
            fallback_reason,
            selected_quality,
            resolved_quality,
            width,
            height,
        }
    }
}

/// Layout metadata for app-produced, row-major timeline thumbnails.
///
/// The app owns decoding and texture upload. This describes the immutable GPU image so the UI
/// can paint clip strips without reading source media on the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoStripLayout {
    pub duration: Tick,
    pub frame_count: usize,
    pub columns: usize,
    pub rows: usize,
    pub frame_width: u32,
    pub frame_height: u32,
}

/// Safety ceiling matching the minimum 2D texture limit of modern wgpu adapters. Ordinary Full
/// preview is viewer-pixel exact; this guard only prevents pathological/off-screen allocations.
const MAX_MONITOR_DIMENSION: u32 = 8_192;
const MONITOR_SIZE_QUANTUM: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
    Unknown,
}

/// A validated project frame rate retained by the editor session.
///
/// The project document owns persistence for this value; editor snapshots deliberately do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectFrameRate {
    numerator: u32,
    denominator: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidProjectFrameRate;

impl fmt::Display for InvalidProjectFrameRate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("project frame rate numerator and denominator must be non-zero")
    }
}

impl std::error::Error for InvalidProjectFrameRate {}

impl ProjectFrameRate {
    pub const DEFAULT: Self = Self {
        numerator: 30,
        denominator: 1,
    };

    pub fn new(numerator: u32, denominator: u32) -> Result<Self, InvalidProjectFrameRate> {
        (numerator != 0 && denominator != 0)
            .then_some(Self {
                numerator,
                denominator,
            })
            .ok_or(InvalidProjectFrameRate)
    }

    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    pub const fn denominator(self) -> u32 {
        self.denominator
    }

    fn frame_index_at_tick(self, tick: Tick) -> u64 {
        let tick = tick.0.max(0) as u128;
        let frames = tick.saturating_mul(self.numerator as u128)
            / (1_000_000_u128 * self.denominator as u128);
        frames.min(u64::MAX as u128) as u64
    }

    fn frame_boundary_tick(self, frame_index: u64) -> Tick {
        let micros = (frame_index as u128)
            .saturating_mul(1_000_000)
            .saturating_mul(self.denominator as u128);
        let tick = micros.saturating_add(self.numerator as u128 - 1) / self.numerator as u128;
        Tick(tick.min(i64::MAX as u128) as i64)
    }

    fn frame_before_end(self, duration: Tick) -> Tick {
        let duration = duration.0.max(0) as u128;
        if duration == 0 {
            return Tick(0);
        }
        let divisor = 1_000_000_u128 * self.denominator as u128;
        let frame_count = duration
            .saturating_mul(self.numerator as u128)
            .saturating_add(divisor - 1)
            / divisor;
        self.frame_boundary_tick(frame_count.saturating_sub(1).min(u64::MAX as u128) as u64)
    }

    fn display_frames_per_second(self) -> u64 {
        ((self.numerator as u64 + self.denominator as u64 / 2) / self.denominator as u64).max(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineTool {
    Pointer,
    Trim,
    Razor,
    Slip,
    DynamicTrim,
    Range,
}

/// The active project workspace. Both views operate on the same authoritative timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorWorkspace {
    #[default]
    Edit,
    Undertow,
    KrakenUpscale,
}

#[derive(Clone, Copy, Debug)]
enum EditorEditMode {
    Insert,
    Overwrite,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorCommand {
    Undo,
    Redo,
    AddVideoTrack,
    AddAudioTrack,
    RazorAtPlayhead,
    DeleteSelected,
    PointerTool,
    RangeTool,
}

#[derive(Clone, Copy)]
struct CommandSpec {
    command: EditorCommand,
    id: &'static str,
    english: &'static str,
    japanese: &'static str,
    key: Option<&'static str>,
}

const EDITOR_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: EditorCommand::Undo,
        id: "edit.undo",
        english: "Undo",
        japanese: "元に戻す",
        key: Some("Ctrl+Z"),
    },
    CommandSpec {
        command: EditorCommand::Redo,
        id: "edit.redo",
        english: "Redo",
        japanese: "やり直す",
        key: Some("Ctrl+Y"),
    },
    CommandSpec {
        command: EditorCommand::AddVideoTrack,
        id: "track.add-video",
        english: "Add video track",
        japanese: "ビデオトラックを追加",
        key: None,
    },
    CommandSpec {
        command: EditorCommand::AddAudioTrack,
        id: "track.add-audio",
        english: "Add audio track",
        japanese: "オーディオトラックを追加",
        key: None,
    },
    CommandSpec {
        command: EditorCommand::RazorAtPlayhead,
        id: "edit.razor-playhead",
        english: "Razor selected track at playhead",
        japanese: "再生ヘッドで選択トラックを分割",
        key: Some("Ctrl+B"),
    },
    CommandSpec {
        command: EditorCommand::DeleteSelected,
        id: "edit.delete-selected",
        english: "Delete selected clip",
        japanese: "選択クリップを削除",
        key: Some("Delete"),
    },
    CommandSpec {
        command: EditorCommand::PointerTool,
        id: "tool.pointer",
        english: "Pointer tool",
        japanese: "選択ツール",
        key: Some("A"),
    },
    CommandSpec {
        command: EditorCommand::RangeTool,
        id: "tool.range",
        english: "Range selection tool",
        japanese: "範囲選択ツール",
        key: Some("R"),
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorAction {
    ChooseMediaFiles,
    ReturnToHub,
    StartExport,
    CancelExport,
    SetForceSoftwareDecode(bool),
    #[cfg(debug_assertions)]
    SetVsync(bool),
    AnalyzeMedia {
        media_id: MediaId,
        path: PathBuf,
    },
    StartKrakenUpscale,
    CancelKrakenUpscale,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EditorExportStatus {
    Idle,
    Running { progress: f32 },
    Completed(PathBuf),
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct CachedWaveform {
    pub duration: Tick,
    /// One min/max pair per decoded bucket. This UI never reads media files.
    pub peaks: Vec<(f32, f32)>,
    /// Unscaled source magnitude per bucket for meter display.
    pub meter_peaks: Vec<f32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<usize>,
}

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
    pub frame_rate_ratio: Option<SourceFrameRate>,
    pub video_bit_rate: Option<u64>,
    pub audio_codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<usize>,
    pub audio_bit_rate: Option<u64>,
    pub streams: Vec<MediaStreamMetadata>,
}

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
    pub frame_rate_ratio: Option<SourceFrameRate>,
    pub sample_rate: Option<u32>,
    pub channels: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TimelineDrag {
    /// The ruler handle owns this drag so scrubbing remains active after the pointer crosses
    /// clips or track controls.
    Scrub,
    Move {
        clip_id: ClipId,
        grab_offset: Tick,
    },
    TitleMove {
        title_id: TitleId,
        grab_offset: Tick,
    },
    TitleTrim {
        title_id: TitleId,
        edge: FadeEdge,
    },
    Gain(ClipId),
    FadeDuration(ClipId, FadeEdge),
    FadeCurve(ClipId, FadeEdge),
    ColorKeyframe(TimelineColorKeyframe),
    Trim {
        clip_id: ClipId,
        edge: FadeEdge,
        last_tick: Tick,
        ripple: bool,
    },
    Roll {
        left_clip: ClipId,
        right_clip: ClipId,
        last_tick: Tick,
    },
    Slip {
        clip_id: ClipId,
        last_tick: Tick,
    },
    ResizeTrack {
        track_id: TrackId,
        start_height: f32,
    },
    Range {
        anchor: Tick,
    },
    Pan,
}

/// A timeline-facing handle for a durable source-time color keyframe.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TimelineColorKeyframe {
    clip_id: ClipId,
    effect_id: VideoEffectId,
    parameter: ColorParameter,
    source_tick: Tick,
    /// Pointer-to-key offset captured on press so a near-edge grab never jumps the key.
    grab_offset: Tick,
    value: f32,
    interpolation: KeyframeInterpolation,
}

struct TimelineClipPaint<'a> {
    waveform: Option<&'a CachedWaveform>,
    waveform_status_galley: Option<Arc<egui::Galley>>,
    waveform_status_color: Color32,
    offline: bool,
    video_strip: Option<CachedVideoStrip>,
    show_video_thumbnails: bool,
    show_audio_waveforms: bool,
    flag_color: Option<Color32>,
    label_galley: Option<Arc<egui::Galley>>,
    offline_prefix_galley: Option<Arc<egui::Galley>>,
    selected: bool,
    enabled: bool,
    show_handles: bool,
}

#[derive(Clone, Copy, Debug)]
struct MediaDragPayload {
    media_id: MediaId,
}

/// Runtime-only catalog item carried from the Transitions sidebar to the timeline.
/// It intentionally contains no timeline identity: a transition is only valid once a
/// particular visible cut has been resolved at drop time.
#[derive(Clone, Copy, Debug)]
struct TransitionDragPayload {
    kind: VideoTransitionKind,
}

#[derive(Clone, Copy, Debug)]
struct CachedVideoStrip {
    native_texture_id: u64,
    texture: egui::TextureId,
    layout: VideoStripLayout,
}

#[derive(Clone, Debug, Default)]
struct TimelineMediaDrawSlot {
    waveform: Option<Arc<CachedWaveform>>,
    waveform_failed: bool,
    offline: bool,
    video_strip: Option<CachedVideoStrip>,
    flag_color: Option<Color32>,
}

#[derive(Clone)]
struct CachedTitleTexture {
    title: TitleOverlay,
    texture: egui::TextureHandle,
    size: [usize; 2],
}

impl fmt::Debug for CachedTitleTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedTitleTexture")
            .field("title", &self.title)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
struct TimelineDropGeometry {
    rect: Rect,
    content: Rect,
    view_start: Tick,
    visible_ticks: f32,
}

#[derive(Clone, Copy, Debug)]
struct TimelineTrackRowGeometry {
    track_id: TrackId,
    kind: TrackKind,
    rect: Rect,
}

#[derive(Clone, Debug)]
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,
    pub kind: MediaKind,
    pub duration: Option<Tick>,
    display_name: String,
    search_name: String,
    label: String,
}

#[derive(Clone, Copy, Debug)]
struct PerformanceHudMetrics {
    frame_ms: f32,
    p95_ms: f32,
    native_rects: usize,
    native_textures: usize,
}

/// Session-only diagnostics supplied by the application. These counters are never persisted and
/// are displayed in the performance HUD's hover panel instead of widening the window border.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RuntimeDiagnostics {
    pub monitor_requests: u64,
    pub monitor_completed_frames: u64,
    pub monitor_presented_frames: u64,
    pub monitor_dropped_frames: u64,
    pub monitor_hold_events: u64,
    pub monitor_late_frames: u64,
    pub monitor_errors: u64,
    pub monitor_turnaround_p95_ms: f32,
    pub native_viewer_uploads: u64,
    pub fallback_viewer_uploads: u64,
    pub audio_underrun_frames: u64,
    pub audio_callback_lock_failures: u64,
    pub audio_late_discarded_frames: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PerformanceHudSummary {
    clip_count: usize,
    view_start: Tick,
    view_span: Tick,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProvisionalTimingState {
    clip_ids: HashSet<ClipId>,
}

#[derive(Clone, Debug)]
struct EditorHistoryCheckpoint {
    timeline: TimelineSnapshot,
    provisional: ProvisionalTimingState,
}

#[derive(Clone, Debug)]
struct ProvisionalHistoryEntry {
    before: HashSet<ClipId>,
    after: HashSet<ClipId>,
}

#[derive(Clone, Debug, Default)]
struct EditorUndoStack {
    timeline: UndoStack,
    undo: Vec<ProvisionalHistoryEntry>,
    redo: Vec<ProvisionalHistoryEntry>,
}

impl EditorUndoStack {
    const CAPACITY: usize = 256;

    fn record(
        &mut self,
        before: &TimelineSnapshot,
        after: &Timeline,
        before_provisional: ProvisionalTimingState,
        after_provisional: ProvisionalTimingState,
    ) -> bool {
        if !self.timeline.record_current(before, after) {
            return false;
        }
        if self.undo.len() == Self::CAPACITY {
            self.undo.remove(0);
        }
        self.undo.push(ProvisionalHistoryEntry {
            before: before_provisional.clip_ids,
            after: after_provisional.clip_ids,
        });
        self.redo.clear();
        true
    }

    fn undo(&mut self, timeline: &mut Timeline) -> Option<HashSet<ClipId>> {
        if !self.timeline.undo(timeline) {
            return None;
        }
        let entry = self
            .undo
            .pop()
            .expect("editor and timeline undo histories stay synchronized");
        let state = entry.before.clone();
        self.redo.push(entry);
        Some(state)
    }

    fn redo(&mut self, timeline: &mut Timeline) -> Option<HashSet<ClipId>> {
        if !self.timeline.redo(timeline) {
            return None;
        }
        let entry = self
            .redo
            .pop()
            .expect("editor and timeline redo histories stay synchronized");
        let state = entry.after.clone();
        self.undo.push(entry);
        Some(state)
    }

    fn can_undo(&self) -> bool {
        self.timeline.can_undo()
    }

    fn can_redo(&self) -> bool {
        self.timeline.can_redo()
    }
}

#[derive(Clone, Debug)]
pub struct EditorState {
    pub language: Language,
    pub project_name: String,
    pub frame_rate: ProjectFrameRate,
    pub media: Vec<MediaItem>,
    pub selected_media: Option<MediaId>,
    pub search: String,
    pub drop_hovered: bool,
    pub tool: TimelineTool,
    inspector_scale_linked: bool,
    pub workspace: EditorWorkspace,
    /// Runtime-only focus for Undertow's compact multi-track selector.
    undertow_track: Option<TrackId>,
    pub range_selection: Option<(Tick, Tick)>,
    pub playing: bool,
    pub export_status: EditorExportStatus,
    pub kraken_upscale_ready: bool,
    pub kraken_upscale_reason: String,
    pub kraken_upscale_quality: u8,
    pub kraken_upscale_goal: u8,
    pub kraken_upscale_status: EditorExportStatus,
    pub show_licenses: bool,
    pub force_software_decode: bool,
    #[cfg(debug_assertions)]
    pub vsync_enabled: bool,
    command_palette_open: bool,
    command_query: String,
    held_razor: bool,
    /// Timeline time in microseconds. This is intentionally decoder-independent.
    pub playhead: Tick,
    pub timeline: Timeline,
    history: EditorUndoStack,
    pending_history: Option<EditorHistoryCheckpoint>,
    timeline_cache: TimelineCache,
    timeline_draw_records: Vec<TrackDrawRecord>,
    pub selected_timeline_clip: Option<ClipId>,
    pub selected_title: Option<TitleId>,
    /// Runtime-only Inspector focus for the compact timeline keyframe overlay.
    active_color_effect: Option<VideoEffectId>,
    pub snapping: bool,
    pub linked_selection: bool,
    pub position_lock: bool,
    pub show_video_thumbnails: bool,
    pub show_audio_waveforms: bool,
    /// Durable user preference for live viewer decode resolution.
    preview_quality: PreviewQuality,
    /// Durable user preference for the paused viewer decode resolution.
    paused_preview_quality: PreviewQuality,
    /// Durable opt-in/out for the high-quality playback path.
    high_quality_playback: bool,
    /// Runtime-only resolution selected by the adaptive decode policy when Auto is selected.
    auto_preview_quality: PreviewQuality,
    pub track_density: TimelineTrackDensity,
    pub markers: Vec<TimelineMarker>,
    pub flags: Vec<TimelineFlag>,
    /// Normalized left/right handles. Wider separation means a wider time range.
    pub zoom_left: f32,
    pub zoom_right: f32,
    /// Editor workspace proportions are deliberately local UI state, so a drag stays put for
    /// the lifetime of this editor session without leaking into timeline data.
    pub media_pool_width: f32,
    pub analysis_width: f32,
    /// Active right-sidebar view is session-only UI state. It is intentionally excluded from
    /// snapshots and durable generations so inspecting media never dirties a project.
    right_sidebar_tab: RightSidebarTab,
    /// Runtime-only target retained while a timeline clip context menu is open.
    timeline_context_clip: Option<ClipId>,
    pub undertow_tools_width: f32,
    pub undertow_mixer_width: f32,
    pub timeline_height: f32,
    timeline_height_is_default: bool,
    /// Last resolved upper-workspace and timeline panel heights. Runtime package evidence reads
    /// this after layout; it is deliberately excluded from project persistence.
    rendered_panel_heights: Option<(f32, f32)>,
    /// Pixel offset into the vertically scrollable track canvas.
    pub timeline_scroll_y: f32,
    /// Visible timeline interval. Unlike the old zoom-only view, this can pan past 300 seconds.
    pub timeline_view_start: Tick,
    pub timeline_view_span: Tick,
    provisional_clip_ids: HashSet<ClipId>,
    auto_fit_provisional_view: bool,
    /// Per-track display heights. Tracks not present here use the default height.
    track_heights: HashMap<TrackId, f32>,
    /// Retained decoded frames in the same bottom-to-top order as `playback_targets`.
    monitor_layers: [Option<MonitorFrame>; PREVIEW_VIDEO_LAYER_COUNT],
    /// Runtime-only decode evidence in the same bottom-to-top order as `monitor_layers`.
    active_preview_diagnostics: [Option<ActivePreviewDiagnostic>; PREVIEW_VIDEO_LAYER_COUNT],
    /// Compatibility mirror for callers that still consume one monitor frame.
    pub monitor: Option<MonitorFrame>,
    pub monitor_status: MonitorStatus,
    performance_hud: String,
    performance_hud_metrics: Option<PerformanceHudMetrics>,
    performance_hud_summary: Option<PerformanceHudSummary>,
    runtime_diagnostics: RuntimeDiagnostics,
    audio_meter_levels: (f32, f32),
    audio_output_error: Option<String>,
    media_paths: HashSet<PathBuf>,
    filtered_media: Vec<usize>,
    filter_query: String,
    next_media_id: MediaId,
    waveforms: HashMap<MediaId, Arc<CachedWaveform>>,
    waveform_errors: HashMap<MediaId, String>,
    media_errors: HashMap<MediaId, String>,
    media_metadata: HashMap<MediaId, MediaMetadata>,
    /// Packet timing is rebuildable runtime state and is deliberately never serialized.
    source_frame_time_indexes: HashMap<MediaId, SourceFrameTimeIndex>,
    media_decoder_backends: HashMap<MediaId, String>,
    video_strips: HashMap<MediaId, CachedVideoStrip>,
    /// Retained text layouts indexed by media-vector position. The timeline hot loop only clones
    /// an Arc; it never formats or allocates one String per visible clip.
    timeline_label_galleys: Vec<Option<Arc<egui::Galley>>>,
    timeline_offline_prefix_galley: Option<Arc<egui::Galley>>,
    timeline_waveform_pending_galley: Option<Arc<egui::Galley>>,
    timeline_waveform_failed_galley: Option<Arc<egui::Galley>>,
    timeline_label_pixels_per_point: f32,
    /// Compact media-ID slots consumed by the visible-clip loop. Hash maps and durable flag
    /// searches are resolved only when their source state changes, never once per clip/frame.
    timeline_media_draw_slots: Vec<TimelineMediaDrawSlot>,
    timeline_media_draw_slots_dirty: bool,
    title_textures: HashMap<TitleId, CachedTitleTexture>,
    title_text_drafts: HashMap<TitleId, String>,
    /// The project raster used to resolve professional viewer transforms.  It deliberately
    /// remains runtime configuration rather than editor-view persistence: project documents own
    /// their output format and may configure it after the editor restores.
    project_canvas_size: (u32, u32),
    monitor_decode_size: (u32, u32),
    timeline_drag: Option<TimelineDrag>,
    /// Media Pool drag ownership independent of egui's cross-panel payload lifetime.
    active_media_drag: Option<MediaId>,
    /// Transition catalog drag ownership independent of egui's cross-panel payload lifetime.
    active_transition_drag: Option<VideoTransitionKind>,
    timeline_drop_geometry: Option<TimelineDropGeometry>,
    /// Visible rows from the most recent timeline frame. Transition drops deliberately resolve
    /// against this geometry so they can never land on an off-screen or audio track.
    timeline_track_rows: Vec<TimelineTrackRowGeometry>,
    /// Previous-frame row geometry lets the editor claim a press before child widgets or the
    /// ScrollArea consume the gesture on Windows.
    media_drag_rects: HashMap<MediaId, Rect>,
    action: Option<EditorAction>,
    /// Monotonic revision of state represented by `EditorProjectSnapshot`.
    /// Runtime playback/decode state intentionally never touches this counter.
    durable_generation: u64,
}

impl EditorState {
    pub fn new(language: Language, project_name: impl Into<String>) -> Self {
        Self::new_with_frame_rate(language, project_name, ProjectFrameRate::DEFAULT)
    }

    pub fn new_with_frame_rate(
        language: Language,
        project_name: impl Into<String>,
        frame_rate: ProjectFrameRate,
    ) -> Self {
        let mut performance_hud = String::with_capacity(256);
        performance_hud.push_str("CPU -- ms · p95 -- ms");
        Self {
            language,
            project_name: project_name.into(),
            frame_rate,
            media: Vec::new(),
            selected_media: None,
            search: String::new(),
            drop_hovered: false,
            tool: TimelineTool::Pointer,
            inspector_scale_linked: true,
            workspace: EditorWorkspace::Edit,
            undertow_track: None,
            range_selection: None,
            playing: false,
            export_status: EditorExportStatus::Idle,
            kraken_upscale_ready: false,
            kraken_upscale_reason: String::new(),
            kraken_upscale_quality: 3,
            kraken_upscale_goal: 2,
            kraken_upscale_status: EditorExportStatus::Idle,
            show_licenses: false,
            force_software_decode: false,
            #[cfg(debug_assertions)]
            vsync_enabled: true,
            command_palette_open: false,
            command_query: String::new(),
            held_razor: false,
            playhead: Tick(0),
            timeline: Timeline::new_default(),
            history: EditorUndoStack::default(),
            pending_history: None,
            timeline_cache: TimelineCache::new(),
            timeline_draw_records: Vec::with_capacity(1_024),
            selected_timeline_clip: None,
            selected_title: None,
            active_color_effect: None,
            snapping: true,
            linked_selection: true,
            position_lock: false,
            show_video_thumbnails: true,
            show_audio_waveforms: true,
            preview_quality: PreviewQuality::Full,
            paused_preview_quality: PreviewQuality::Full,
            high_quality_playback: true,
            auto_preview_quality: PreviewQuality::Full,
            track_density: TimelineTrackDensity::Normal,
            markers: Vec::new(),
            flags: Vec::new(),
            zoom_left: 0.08,
            zoom_right: 0.92,
            media_pool_width: DEFAULT_MEDIA_POOL_WIDTH,
            analysis_width: DEFAULT_RIGHT_SIDEBAR_WIDTH,
            right_sidebar_tab: RightSidebarTab::Inspector,
            timeline_context_clip: None,
            undertow_tools_width: 190.0,
            undertow_mixer_width: 220.0,
            timeline_height: DEFAULT_TIMELINE_HEIGHT,
            timeline_height_is_default: true,
            rendered_panel_heights: None,
            timeline_scroll_y: 0.0,
            timeline_view_start: Tick(0),
            timeline_view_span: legacy_zoom_span(0.08, 0.92),
            provisional_clip_ids: HashSet::new(),
            auto_fit_provisional_view: false,
            track_heights: HashMap::new(),
            monitor_layers: [None; PREVIEW_VIDEO_LAYER_COUNT],
            active_preview_diagnostics: [None; PREVIEW_VIDEO_LAYER_COUNT],
            monitor: None,
            monitor_status: MonitorStatus::Empty,
            performance_hud,
            performance_hud_metrics: None,
            performance_hud_summary: None,
            runtime_diagnostics: RuntimeDiagnostics::default(),
            audio_meter_levels: (0.0, 0.0),
            audio_output_error: None,
            media_paths: HashSet::new(),
            filtered_media: Vec::new(),
            filter_query: String::new(),
            next_media_id: 1,
            waveforms: HashMap::new(),
            waveform_errors: HashMap::new(),
            media_errors: HashMap::new(),
            media_metadata: HashMap::new(),
            source_frame_time_indexes: HashMap::new(),
            media_decoder_backends: HashMap::new(),
            video_strips: HashMap::new(),
            timeline_label_galleys: Vec::new(),
            timeline_offline_prefix_galley: None,
            timeline_waveform_pending_galley: None,
            timeline_waveform_failed_galley: None,
            timeline_label_pixels_per_point: 0.0,
            timeline_media_draw_slots: Vec::new(),
            timeline_media_draw_slots_dirty: true,
            title_textures: HashMap::new(),
            title_text_drafts: HashMap::new(),
            project_canvas_size: (1920, 1080),
            monitor_decode_size: (640, 360),
            timeline_drag: None,
            active_media_drag: None,
            active_transition_drag: None,
            timeline_drop_geometry: None,
            timeline_track_rows: Vec::new(),
            media_drag_rects: HashMap::new(),
            action: None,
            durable_generation: 1,
        }
    }

    /// Conservative one-frame tolerance, rounded up to the integer project tick grid.
    /// Fractional rates can alternate between the adjacent integer tick durations.
    pub fn frame_duration_tick(&self) -> Tick {
        self.frame_rate.frame_boundary_tick(1)
    }

    /// Snaps a project tick to the start of the containing project frame.
    pub fn quantize_tick_to_frame_start(&self, tick: Tick) -> Tick {
        self.frame_rate
            .frame_boundary_tick(self.frame_rate.frame_index_at_tick(tick))
    }

    /// Cheap autosave predicate. A snapshot is only built after this changes.
    pub fn durable_generation(&self) -> u64 {
        self.durable_generation
    }

    pub fn set_workspace(&mut self, workspace: EditorWorkspace) {
        if workspace == EditorWorkspace::KrakenUpscale && !self.kraken_upscale_ready {
            return;
        }
        self.workspace = workspace;
        if workspace == EditorWorkspace::Undertow {
            self.ensure_undertow_track();
        }
    }

    pub fn set_kraken_upscale_capability(&mut self, ready: bool, reason: impl Into<String>) {
        self.kraken_upscale_ready = ready;
        self.kraken_upscale_reason = reason.into();
        if !ready && self.workspace == EditorWorkspace::KrakenUpscale {
            self.workspace = EditorWorkspace::Edit;
        }
    }

    pub fn kraken_source_path(&self) -> Option<PathBuf> {
        if let Some(item) = self.selected() {
            return Some(item.path.clone());
        }
        self.selected_timeline_clip
            .and_then(|clip_id| self.timeline.clip(clip_id))
            .and_then(|clip| {
                self.media
                    .iter()
                    .find(|item| item.id == clip.media.0)
                    .map(|item| item.path.clone())
            })
    }

    pub fn set_kraken_upscale_running(&mut self, progress: f32) {
        self.kraken_upscale_status = EditorExportStatus::Running {
            progress: progress.clamp(0.0, 1.0),
        };
    }

    pub fn set_kraken_upscale_idle(&mut self) {
        self.kraken_upscale_status = EditorExportStatus::Idle;
    }

    pub fn set_kraken_upscale_completed(&mut self, path: PathBuf) {
        self.kraken_upscale_status = EditorExportStatus::Completed(path);
    }

    pub fn set_kraken_upscale_failed(&mut self, error: impl Into<String>) {
        self.kraken_upscale_status = EditorExportStatus::Failed(error.into());
    }

    fn ensure_undertow_track(&mut self) -> Option<TrackId> {
        let selected_audio_track = self
            .selected_timeline_clip
            .and_then(|clip_id| self.timeline.clip(clip_id))
            .and_then(|clip| {
                self.timeline
                    .track(clip.track_id)
                    .filter(|track| track.kind == TrackKind::Audio)
                    .map(|track| track.id)
            });
        let current_is_valid = self.undertow_track.is_some_and(|track_id| {
            self.timeline
                .track(track_id)
                .is_some_and(|track| track.kind == TrackKind::Audio)
        });
        if !current_is_valid {
            self.undertow_track = selected_audio_track.or_else(|| {
                self.timeline
                    .tracks
                    .iter()
                    .find(|track| track.kind == TrackKind::Audio)
                    .map(|track| track.id)
            });
        }
        self.undertow_track
    }

    fn focus_undertow_track(&mut self, track_id: TrackId) {
        if !self
            .timeline
            .track(track_id)
            .is_some_and(|track| track.kind == TrackKind::Audio)
        {
            return;
        }
        self.undertow_track = Some(track_id);
        let presentation = TimelinePresentation {
            show_tool_row: false,
            audio_focus: Some(track_id),
        };
        self.timeline_scroll_y = self
            .timeline
            .tracks
            .iter()
            .take_while(|track| track.id != track_id)
            .map(|track| {
                let stored = self
                    .track_heights
                    .get(&track.id)
                    .copied()
                    .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
                presented_track_height(track.kind, track.id, stored, presentation)
            })
            .sum::<f32>();
    }

    /// Returns the actual viewer-workspace and timeline panel heights from the latest frame.
    pub fn rendered_panel_heights(&self) -> Option<(f32, f32)> {
        self.rendered_panel_heights
    }

    fn mark_durable_edit(&mut self) {
        self.durable_generation = self.durable_generation.wrapping_add(1).max(1);
    }

    fn mark_changed_timeline_generation(&mut self, previous: u64) -> bool {
        let changed = self.timeline.generation() != previous;
        if changed {
            self.mark_durable_edit();
        }
        changed
    }

    fn provisional_timing_state(&self) -> ProvisionalTimingState {
        ProvisionalTimingState {
            clip_ids: self.provisional_clip_ids.clone(),
        }
    }

    fn restore_provisional_history_ids(&mut self, clip_ids: HashSet<ClipId>) {
        self.provisional_clip_ids = clip_ids;
        if self.provisional_clip_ids.is_empty() {
            self.auto_fit_provisional_view = false;
        } else if self.timeline_view_matches_full_extent() {
            self.auto_fit_provisional_view = true;
        }
    }

    fn reconcile_restored_known_durations(&mut self) {
        let known = self
            .provisional_clip_ids
            .iter()
            .filter_map(|clip_id| self.timeline.clip(*clip_id))
            .filter_map(|clip| {
                self.media
                    .iter()
                    .find(|item| item.id == clip.media.0)
                    .and_then(|item| item.duration.map(|duration| (item.id, duration)))
            })
            .collect::<HashSet<_>>();
        for (media_id, duration) in known {
            let _ = self.reconcile_media_duration(media_id, duration);
        }
    }

    fn timeline_history_checkpoint(&self) -> EditorHistoryCheckpoint {
        EditorHistoryCheckpoint {
            timeline: self.timeline.snapshot(),
            provisional: self.provisional_timing_state(),
        }
    }

    fn begin_timeline_history(&mut self) {
        if self.pending_history.is_none() {
            self.pending_history = Some(self.timeline_history_checkpoint());
        }
    }

    fn commit_timeline_history(&mut self) -> bool {
        let Some(before) = self.pending_history.take() else {
            return false;
        };
        self.record_timeline_history(before)
    }

    fn record_timeline_history(&mut self, before: EditorHistoryCheckpoint) -> bool {
        let after_provisional = self.provisional_timing_state();
        self.history.record(
            &before.timeline,
            &self.timeline,
            before.provisional,
            after_provisional,
        )
    }

    fn abandon_provisional_timing<I>(&mut self, clip_ids: I) -> bool
    where
        I: IntoIterator<Item = ClipId>,
    {
        let before = self.provisional_clip_ids.len();
        for clip_id in clip_ids {
            self.provisional_clip_ids.remove(&clip_id);
        }
        if self.provisional_clip_ids.is_empty() {
            self.auto_fit_provisional_view = false;
        }
        self.provisional_clip_ids.len() != before
    }

    fn abandon_changed_provisional_since(&mut self, before: &TimelineSnapshot) {
        let changed = self
            .provisional_clip_ids
            .iter()
            .copied()
            .filter(|clip_id| {
                let prior = before
                    .tracks
                    .iter()
                    .flat_map(|track| &track.clips)
                    .find(|clip| clip.id == *clip_id);
                match (prior, self.timeline.clip(*clip_id)) {
                    (Some(prior), Some(current)) => {
                        prior.media != current.media
                            || prior.duration != current.duration
                            || prior.source_in != current.source_in
                    }
                    _ => true,
                }
            })
            .collect::<Vec<_>>();
        self.abandon_provisional_timing(changed);
    }

    pub fn undo_timeline(&mut self) -> bool {
        self.pending_history = None;
        let Some(provisional) = self.history.undo(&mut self.timeline) else {
            return false;
        };
        self.restore_provisional_history_ids(provisional);
        self.reconcile_restored_known_durations();
        if self
            .selected_timeline_clip
            .is_some_and(|id| self.timeline.clip(id).is_none())
        {
            self.selected_timeline_clip = None;
        }
        if self
            .selected_title
            .is_some_and(|id| self.timeline.title(id).is_none())
        {
            self.selected_title = None;
        }
        self.playing = false;
        self.mark_durable_edit();
        true
    }

    pub fn open_command_palette(&mut self) {
        self.command_palette_open = true;
        self.command_query.clear();
    }

    pub fn redo_timeline(&mut self) -> bool {
        self.pending_history = None;
        let Some(provisional) = self.history.redo(&mut self.timeline) else {
            return false;
        };
        self.restore_provisional_history_ids(provisional);
        self.reconcile_restored_known_durations();
        if self
            .selected_title
            .is_some_and(|id| self.timeline.title(id).is_none())
        {
            self.selected_title = None;
        }
        self.playing = false;
        self.mark_durable_edit();
        true
    }

    pub fn set_audio_output_error(&mut self, error: impl Into<String>) {
        self.audio_output_error = Some(error.into());
    }

    /// Updates runtime peaks captured from samples consumed by the native output callback.
    pub fn set_audio_meter_levels(&mut self, left: f32, right: f32) {
        self.audio_meter_levels = (sanitized_meter_level(left), sanitized_meter_level(right));
    }

    /// Updates the retained performance label outside the UI frame hot path.
    pub fn set_performance_hud(
        &mut self,
        frame_ms: f32,
        p95_ms: f32,
        native_rects: usize,
        native_textures: usize,
    ) {
        self.performance_hud_metrics = Some(PerformanceHudMetrics {
            frame_ms,
            p95_ms,
            native_rects,
            native_textures,
        });
        self.rebuild_performance_hud();
    }

    /// Replaces the non-durable session counters shown behind the retained performance label.
    pub fn set_runtime_diagnostics(&mut self, diagnostics: RuntimeDiagnostics) {
        self.runtime_diagnostics = diagnostics;
    }

    /// Rebuilds the retained HUD only after timeline content or view changes.
    /// Returns whether the caller should schedule one repaint to display it.
    pub fn refresh_performance_hud_if_stale(&mut self) -> bool {
        let Some(_) = self.performance_hud_metrics else {
            return false;
        };
        if self.performance_hud_summary == Some(self.performance_hud_summary()) {
            return false;
        }
        self.rebuild_performance_hud();
        true
    }

    fn performance_hud_summary(&self) -> PerformanceHudSummary {
        PerformanceHudSummary {
            clip_count: self.timeline.clip_count(),
            view_start: self.timeline_view_start,
            view_span: self.timeline_view_span,
        }
    }

    fn rebuild_performance_hud(&mut self) {
        let metrics = self
            .performance_hud_metrics
            .expect("performance HUD rebuild requires captured metrics");
        let summary = self.performance_hud_summary();
        let view_end = Tick(summary.view_start.0.saturating_add(summary.view_span.0));
        self.performance_hud.clear();
        write!(
            self.performance_hud,
            "CPU {:.2} ms · p95 {:.2} ms · {} clips · ",
            metrics.frame_ms, metrics.p95_ms, summary.clip_count,
        )
        .expect("writing to a String cannot fail");
        write_timecode(
            &mut self.performance_hud,
            summary.view_start,
            self.frame_rate,
        );
        self.performance_hud.push('–');
        write_timecode(&mut self.performance_hud, view_end, self.frame_rate);
        write!(
            self.performance_hud,
            " · {}R/{}T",
            metrics.native_rects, metrics.native_textures,
        )
        .expect("writing to a String cannot fail");
        self.performance_hud_summary = Some(summary);
    }

    pub fn clear_audio_output_error(&mut self) {
        self.audio_output_error = None;
    }

    pub fn set_export_running(&mut self, progress: f32) {
        self.export_status = EditorExportStatus::Running {
            progress: progress.clamp(0.0, 1.0),
        };
    }

    pub fn set_export_idle(&mut self) {
        self.export_status = EditorExportStatus::Idle;
    }

    pub fn set_export_completed(&mut self, path: PathBuf) {
        self.export_status = EditorExportStatus::Completed(path);
    }

    pub fn set_export_failed(&mut self, error: impl Into<String>) {
        self.export_status = EditorExportStatus::Failed(error.into());
    }

    /// Keeps Quick Export inside the bounded preview/render contract. Transforms and the first
    /// four visible video tracks share one compositor plan; larger stacks and unsupported audio
    /// processors remain explicit until their export lowering is implemented.
    pub fn quick_export_block_message(&self) -> Option<&'static str> {
        let mut contributing_video_tracks = 0;
        for track in &self.timeline.tracks {
            if track.kind != TrackKind::Video
                || track.muted
                || !track.clips.iter().any(|clip| clip.enabled)
            {
                continue;
            }
            contributing_video_tracks += 1;
            if contributing_video_tracks > PREVIEW_VIDEO_LAYER_COUNT {
                return Some(match self.language {
                    Language::English => {
                        "Quick Export supports up to four visible video layers. Mute an extra video track before exporting."
                    }
                    Language::Japanese => {
                        "クイック書き出しは表示中の映像レイヤー4つまで対応します。追加の映像トラックをミュートしてから書き出してください。"
                    }
                });
            }
        }
        let any_audio_solo = self
            .timeline
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Audio && track.solo);
        if self.timeline.tracks.iter().any(|track| {
            track.audio_is_audible(any_audio_solo)
                && track.clips.iter().any(|clip| clip.enabled)
                && (track.effects.iter().any(audio_effect_blocks_export)
                    || track
                        .clips
                        .iter()
                        .filter(|clip| clip.enabled)
                        .flat_map(|clip| &clip.effects)
                        .any(audio_effect_blocks_export))
        }) {
            return Some(match self.language {
                Language::English => {
                    "Quick Export cannot render one or more unsupported audio effects. Bypass or remove the highlighted effects before exporting."
                }
                Language::Japanese => {
                    "クイック書き出しで未対応のオーディオエフェクトがあります。強調表示されたエフェクトをバイパスまたは削除してください。"
                }
            });
        }
        None
    }

    pub fn snapshot(&self) -> EditorProjectSnapshot {
        let mut track_heights = self
            .track_heights
            .iter()
            .map(|(&track_id, &height)| TrackHeightSnapshot { track_id, height })
            .collect::<Vec<_>>();
        track_heights.sort_by_key(|entry| entry.track_id.0);
        EditorProjectSnapshot {
            version: EDITOR_PROJECT_SNAPSHOT_VERSION,
            media: self
                .media
                .iter()
                .map(|item| EditorMediaSnapshot {
                    id: item.id,
                    path: item.path.clone(),
                    duration: item.duration,
                })
                .collect(),
            timeline: self.timeline.snapshot(),
            view: EditorViewSnapshot {
                playhead: self.playhead,
                selected_media: self.selected_media,
                selected_timeline_clip: self.selected_timeline_clip,
                selected_title: self.selected_title,
                zoom_left: self.zoom_left,
                zoom_right: self.zoom_right,
                media_pool_width: self.media_pool_width,
                analysis_width: self.analysis_width,
                undertow_tools_width: self.undertow_tools_width,
                undertow_mixer_width: self.undertow_mixer_width,
                timeline_height: self.timeline_height,
                timeline_height_is_default: Some(self.timeline_height_is_default),
                timeline_scroll_y: self.timeline_scroll_y,
                track_heights,
                timeline_view_start: self.timeline_view_start,
                timeline_view_span: self.timeline_view_span,
                snapping: self.snapping,
                linked_selection: self.linked_selection,
                position_lock: self.position_lock,
                show_video_thumbnails: self.show_video_thumbnails,
                show_audio_waveforms: self.show_audio_waveforms,
                preview_quality: self.preview_quality,
                preview_quality_is_explicit: true,
                paused_preview_quality: self.paused_preview_quality,
                high_quality_playback: self.high_quality_playback,
                track_density: self.track_density,
                markers: self.markers.clone(),
                flags: self.flags.clone(),
                provisional_clip_ids: Some({
                    let mut ids = self
                        .provisional_clip_ids
                        .iter()
                        .copied()
                        .collect::<Vec<_>>();
                    ids.sort_by_key(|id| id.0);
                    ids
                }),
                auto_fit_provisional_view: Some(self.auto_fit_provisional_view),
            },
        }
    }

    /// Rebuilds durable editor state while resetting all GPU, decoding, action, and playback
    /// state. Snapshot data is validated before this state is changed.
    pub fn restore(
        language: Language,
        project_name: impl Into<String>,
        snapshot: EditorProjectSnapshot,
    ) -> Result<Self, EditorRestoreError> {
        Self::restore_with_frame_rate(language, project_name, snapshot, ProjectFrameRate::DEFAULT)
    }

    /// Rebuilds durable editor state using the frame rate supplied by the owning project document.
    pub fn restore_with_frame_rate(
        language: Language,
        project_name: impl Into<String>,
        mut snapshot: EditorProjectSnapshot,
        frame_rate: ProjectFrameRate,
    ) -> Result<Self, EditorRestoreError> {
        if snapshot.version != EDITOR_PROJECT_SNAPSHOT_VERSION {
            return Err(EditorRestoreError::UnsupportedVersion(snapshot.version));
        }
        // Every media hot path uses the compact `id - 1` slot directly. Portable JSON may
        // reorder its catalog entries, so normalize that harmless difference here; a genuine ID
        // gap is rejected before any partially usable editor state can be constructed.
        snapshot.media.sort_unstable_by_key(|item| item.id);
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        for item in &snapshot.media {
            if item.id == 0 || item.id == MediaId::MAX {
                return Err(EditorRestoreError::InvalidMediaId(item.id));
            }
            if !ids.insert(item.id) {
                return Err(EditorRestoreError::DuplicateMediaId(item.id));
            }
            if !paths.insert(item.path.clone()) {
                return Err(EditorRestoreError::DuplicateMediaPath(item.path.clone()));
            }
        }
        for (index, item) in snapshot.media.iter().enumerate() {
            let expected = MediaId::try_from(index)
                .unwrap_or(MediaId::MAX)
                .saturating_add(1);
            if item.id != expected {
                return Err(EditorRestoreError::NonContiguousMediaId {
                    expected,
                    actual: item.id,
                });
            }
        }
        let timeline = Timeline::from_snapshot(snapshot.timeline)
            .map_err(EditorRestoreError::InvalidTimeline)?;
        for clip in timeline.tracks.iter().flat_map(|track| &track.clips) {
            if !ids.contains(&clip.media.0) {
                return Err(EditorRestoreError::UnknownTimelineMedia(clip.media.0));
            }
        }

        let mut state = Self::new_with_frame_rate(language, project_name, frame_rate);
        state.timeline = timeline;
        state.media = snapshot
            .media
            .into_iter()
            .map(|item| media_item(item.id, item.path, item.duration))
            .collect();
        state.media_paths = paths;
        state.next_media_id = state
            .media
            .iter()
            .map(|item| item.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        state.selected_media = snapshot.view.selected_media.filter(|id| ids.contains(id));
        state.selected_timeline_clip = snapshot.view.selected_timeline_clip.filter(|id| {
            state
                .timeline
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .any(|clip| clip.id == *id)
        });
        state.selected_title = snapshot
            .view
            .selected_title
            .filter(|id| state.timeline.title(*id).is_some());
        state.set_playhead(snapshot.view.playhead);
        state.set_zoom_handles(
            finite_or(snapshot.view.zoom_left, 0.08),
            finite_or(snapshot.view.zoom_right, 0.92),
        );
        state.media_pool_width =
            finite_or(snapshot.view.media_pool_width, DEFAULT_MEDIA_POOL_WIDTH).max(190.0);
        state.analysis_width =
            finite_or(snapshot.view.analysis_width, DEFAULT_RIGHT_SIDEBAR_WIDTH).max(220.0);
        state.undertow_tools_width = finite_or(
            snapshot.view.undertow_tools_width,
            default_undertow_tools_width(),
        )
        .max(150.0);
        state.undertow_mixer_width = finite_or(
            snapshot.view.undertow_mixer_width,
            default_undertow_mixer_width(),
        )
        .max(180.0);
        let timeline_height = finite_or(snapshot.view.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        // Pre-marker projects saved former defaults as ordinary view state. Upgrade exact known
        // values only when the marker is absent; explicit custom/default ownership is authoritative.
        let historical_default = snapshot.view.timeline_height_is_default.is_none()
            && matches!(
                timeline_height,
                LEGACY_DEFAULT_TIMELINE_HEIGHT
                    | PREVIOUS_DEFAULT_TIMELINE_HEIGHT
                    | FORMER_DEFAULT_TIMELINE_HEIGHT
                    | PREVIOUS_LAYOUT_DEFAULT_TIMELINE_HEIGHT
            );
        state.timeline_height_is_default = snapshot
            .view
            .timeline_height_is_default
            .unwrap_or(historical_default);
        state.timeline_height = if historical_default {
            DEFAULT_TIMELINE_HEIGHT
        } else {
            timeline_height.max(180.0)
        };
        state.timeline_scroll_y = finite_or(snapshot.view.timeline_scroll_y, 0.0).max(0.0);
        state.timeline_view_start = Tick(snapshot.view.timeline_view_start.0.max(0));
        state.timeline_view_span = if snapshot.view.timeline_view_span.0 > 0 {
            snapshot.view.timeline_view_span
        } else {
            state.timeline_view_start = Tick(0);
            legacy_zoom_span(snapshot.view.zoom_left, snapshot.view.zoom_right)
        };
        let legacy_provisional_ownership = snapshot.view.provisional_clip_ids.is_none();
        state.provisional_clip_ids = snapshot
            .view
            .provisional_clip_ids
            .unwrap_or_else(|| {
                state
                    .timeline
                    .tracks
                    .iter()
                    .flat_map(|track| &track.clips)
                    .filter(|clip| {
                        clip.duration == PROVISIONAL_MEDIA_DURATION
                            && clip.source_in == Tick(0)
                            && state
                                .media
                                .iter()
                                .find(|item| item.id == clip.media.0)
                                .is_some_and(|item| item.duration.is_none())
                    })
                    .map(|clip| clip.id)
                    .collect()
            })
            .into_iter()
            .filter(|id| state.timeline.clip(*id).is_some())
            .collect();
        state.auto_fit_provisional_view =
            snapshot.view.auto_fit_provisional_view.unwrap_or_else(|| {
                legacy_provisional_ownership
                    && state.timeline_view_start == Tick(0)
                    && state.timeline_view_span == Tick(16_000_000)
                    && !state.provisional_clip_ids.is_empty()
            });
        state.snapping = snapshot.view.snapping;
        state.linked_selection = snapshot.view.linked_selection;
        state.position_lock = snapshot.view.position_lock;
        state.show_video_thumbnails = snapshot.view.show_video_thumbnails;
        state.show_audio_waveforms = snapshot.view.show_audio_waveforms;
        state.preview_quality = if !snapshot.view.preview_quality_is_explicit
            && snapshot.view.preview_quality == PreviewQuality::Auto
        {
            PreviewQuality::Full
        } else {
            snapshot.view.preview_quality
        };
        state.paused_preview_quality = snapshot.view.paused_preview_quality;
        state.high_quality_playback = snapshot.view.high_quality_playback;
        state.track_density = snapshot.view.track_density;
        state.markers = snapshot
            .view
            .markers
            .into_iter()
            .map(|mut marker| {
                marker.tick = Tick(marker.tick.0.max(0));
                marker.color %= MARKER_COLORS.len() as u8;
                marker
            })
            .collect();
        state.flags = snapshot
            .view
            .flags
            .into_iter()
            .filter_map(|mut flag| {
                if flag.media_id == 0 {
                    flag.media_id = flag
                        .legacy_clip_id
                        .and_then(|clip_id| state.timeline.clip(clip_id))
                        .map(|clip| clip.media.0)?;
                }
                ids.contains(&flag.media_id).then_some(())?;
                flag.legacy_clip_id = None;
                flag.color %= MARKER_COLORS.len() as u8;
                Some(flag)
            })
            .collect();
        let track_ids = state
            .timeline
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<HashSet<_>>();
        state.track_heights = snapshot
            .view
            .track_heights
            .into_iter()
            .filter(|entry| track_ids.contains(&entry.track_id))
            .map(|entry| {
                (
                    entry.track_id,
                    clamp_track_height(finite_or(entry.height, DEFAULT_TIMELINE_TRACK_HEIGHT)),
                )
            })
            .collect();
        state.refresh_media_filter();
        // Restoring is not a new edit. The first save for an opened project is still requested
        // by the application, then this remains stable through playback frames.
        state.durable_generation = 1;
        // `new` establishes empty monitor/caches/action state and a stopped transport.
        Ok(state)
    }

    /// Adds unique paths only. The application owns selection and file-drop handling.
    pub fn add_media_paths<I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let media_count = self.media.len();
        for path in paths {
            if !self.media_paths.insert(path.clone()) {
                continue;
            }
            let id = self.next_media_id;
            self.next_media_id = self.next_media_id.saturating_add(1);
            self.media.push(media_item(id, path, None));
            if self.selected_media.is_none() {
                self.selected_media = Some(id);
            }
        }
        self.refresh_media_filter();
        if self.media.len() != media_count {
            self.timeline_media_draw_slots_dirty = true;
            self.mark_durable_edit();
        }
    }

    pub fn set_drop_hovered(&mut self, hovered: bool) {
        self.drop_hovered = hovered;
    }

    /// Claims a Media Pool press using retained row geometry from the most recently rendered
    /// frame. This lets the native window event path keep ownership when a ScrollArea consumes
    /// egui's drag gesture on Windows.
    pub fn claim_media_drag_at(&mut self, point: Pos2) -> bool {
        self.active_media_drag = self
            .media_drag_rects
            .iter()
            .find_map(|(&media_id, rect)| rect.contains(point).then_some(media_id));
        self.active_media_drag.is_some()
    }

    /// Completes a native-owned Media Pool drag at a window-local logical position.
    ///
    /// Returns whether a Media Pool drag was active. The drag is always cleared, including when
    /// the release is outside the timeline or placement is rejected.
    pub fn complete_media_drag_at(&mut self, point: Pos2) -> bool {
        self.complete_media_drag_at_any([point])
    }

    /// Completes a drag using the first release position that resolves inside the timeline.
    /// Native Windows input can deliver a newer OS position and a retained winit position with
    /// different timing; accepting either prevents a valid visible drop from being discarded.
    pub fn complete_media_drag_at_any(&mut self, points: impl IntoIterator<Item = Pos2>) -> bool {
        let Some(media_id) = self.active_media_drag.take() else {
            return false;
        };
        if let Some(start) = points
            .into_iter()
            .find_map(|point| self.timeline_drop_start_at(point))
        {
            self.overwrite_media_at(media_id, start);
        }
        true
    }

    /// Exercises the same retained source and destination geometry used by native Media Pool
    /// drags. This is intentionally narrow: packaged acceptance uses it after a real layout pass
    /// so a release cannot ship when the visible card or timeline drop zone is disconnected.
    #[doc(hidden)]
    pub fn exercise_layout_backed_media_drop(&mut self, media_id: MediaId) -> bool {
        let Some(source) = self
            .media_drag_rects
            .get(&media_id)
            .map(|rect| rect.center())
        else {
            return false;
        };
        let Some(destination) = self
            .timeline_drop_geometry
            .map(|geometry| geometry.content.center())
        else {
            return false;
        };
        let before_generation = self.timeline.generation();
        self.claim_media_drag_at(source)
            && self.active_media_drag == Some(media_id)
            && self.complete_media_drag_at(destination)
            && self.timeline.generation() != before_generation
            && self.selected_timeline_clip.is_some_and(|clip_id| {
                self.timeline
                    .clip(clip_id)
                    .is_some_and(|clip| clip.media.0 == media_id)
            })
    }

    /// Cancels a native-owned Media Pool drag when its release position is unavailable.
    pub fn cancel_media_drag(&mut self) -> bool {
        self.active_media_drag.take().is_some()
    }

    /// Cancels a sidebar transition drag when the pointer release is lost (for example when the
    /// editor loses focus). The catalog fallback must never survive into a later unrelated drop.
    pub fn cancel_transition_drag(&mut self) -> bool {
        self.active_transition_drag.take().is_some()
    }

    /// Resolves a window-local logical pointer position against the latest timeline layout.
    /// The application obtains the release position directly from the operating system so an
    /// Explorer/OLE drop never depends on egui's potentially stale pointer state.
    pub fn timeline_drop_start_at(&self, point: Pos2) -> Option<Tick> {
        let geometry = self.timeline_drop_geometry?;
        geometry.rect.contains(point).then(|| {
            media_drop_start(
                &self.timeline,
                snap_timeline_tick(
                    self,
                    timeline_tick_at(
                        point
                            .x
                            .clamp(geometry.content.left(), geometry.content.right()),
                        geometry.content,
                        geometry.view_start,
                        geometry.visible_ticks,
                    ),
                    geometry.visible_ticks,
                    geometry.content.width(),
                    None,
                ),
            )
        })
    }

    /// Accepts worker-produced min/max peaks and reconciles provisional clip length.
    /// Calling this does no media inspection or decoding on the UI thread.
    pub fn set_waveform(
        &mut self,
        media_id: MediaId,
        duration: Tick,
        peaks: Vec<(f32, f32)>,
    ) -> Result<(), TimelineError> {
        self.set_waveform_with_audio_info(media_id, duration, peaks, None, None)
    }

    pub fn set_waveform_with_audio_info(
        &mut self,
        media_id: MediaId,
        duration: Tick,
        peaks: Vec<(f32, f32)>,
        sample_rate: Option<u32>,
        channels: Option<usize>,
    ) -> Result<(), TimelineError> {
        self.reconcile_media_duration(media_id, duration)?;
        let meter_peaks = peaks
            .iter()
            .map(|(low, high)| {
                if low.is_finite() && high.is_finite() {
                    low.abs().max(high.abs()).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect();
        self.waveforms.insert(
            media_id,
            Arc::new(CachedWaveform {
                duration,
                // Peaks are display cache only. Normalize once here, never while painting, so
                // quiet source audio remains editable at its real gain without a tiny waveform.
                peaks: normalize_waveform_display(peaks),
                meter_peaks,
                sample_rate,
                channels,
            }),
        );
        self.waveform_errors.remove(&media_id);
        self.timeline_media_draw_slots_dirty = true;
        Ok(())
    }

    pub fn set_media_metadata(&mut self, media_id: MediaId, metadata: MediaMetadata) {
        let is_still = self
            .media
            .iter()
            .find(|item| item.id == media_id)
            .is_some_and(|item| item.kind == MediaKind::Image);
        if !is_still
            && let Some(duration) = metadata.duration_seconds.and_then(duration_seconds_to_tick)
        {
            let _ = self.reconcile_media_duration(media_id, duration);
        }
        self.media_metadata.insert(media_id, metadata);
        self.media_errors.remove(&media_id);
        self.timeline_media_draw_slots_dirty = true;
    }

    pub fn set_media_error(&mut self, media_id: MediaId, error: impl Into<String>) {
        self.media_metadata.remove(&media_id);
        self.source_frame_time_indexes.remove(&media_id);
        self.media_errors.insert(media_id, error.into());
        self.timeline_media_draw_slots_dirty = true;
    }

    /// Reports whether analysis has marked this imported item offline or unreadable.
    /// This is read-only runtime state, deliberately excluded from project snapshots.
    pub fn media_is_offline(&self, media_id: MediaId) -> bool {
        self.media_errors.contains_key(&media_id)
    }

    /// Replaces or clears packet timing for runtime source-frame addressing without affecting saves.
    pub fn set_media_frame_time_index(
        &mut self,
        media_id: MediaId,
        index: Option<SourceFrameTimeIndex>,
    ) {
        if let Some(index) = index {
            self.source_frame_time_indexes.insert(media_id, index);
        } else {
            self.source_frame_time_indexes.remove(&media_id);
        }
    }

    /// Records the actual decoder used by the live monitor, not a hardware capability guess.
    pub fn set_media_decoder_backend(&mut self, media_id: MediaId, backend: impl Into<String>) {
        self.media_decoder_backends.insert(media_id, backend.into());
    }

    pub fn set_waveform_error(&mut self, media_id: MediaId, error: impl Into<String>) {
        self.waveforms.remove(&media_id);
        self.waveform_errors.insert(media_id, error.into());
        self.timeline_media_draw_slots_dirty = true;
    }

    /// Records an app-produced, row-major video preview atlas. The UI only crops it; it never
    /// reads or decodes the source media.
    pub fn set_video_strip(
        &mut self,
        media_id: MediaId,
        native_texture_id: u64,
        texture: egui::TextureId,
        layout: VideoStripLayout,
    ) {
        let is_still = self
            .media
            .iter()
            .find(|item| item.id == media_id)
            .is_some_and(|item| item.kind == MediaKind::Image);
        if !is_still {
            let _ = self.reconcile_media_duration(media_id, layout.duration);
        }
        self.video_strips.insert(
            media_id,
            CachedVideoStrip {
                native_texture_id,
                texture,
                layout,
            },
        );
        self.timeline_media_draw_slots_dirty = true;
    }

    /// Stores worker-probed duration and resolves only clips whose timing is still probe-owned.
    /// Runtime analysis caches are independent; this scalar survives project restore.
    fn reconcile_media_duration(
        &mut self,
        media_id: MediaId,
        duration: Tick,
    ) -> Result<(), TimelineError> {
        if duration.0 <= 0 {
            return Err(TimelineError::InvalidMediaDuration);
        }
        let media = TimelineMediaId(media_id);
        let provisional = self
            .provisional_clip_ids
            .iter()
            .copied()
            .filter(|clip_id| {
                self.timeline
                    .clip(*clip_id)
                    .is_some_and(|clip| clip.media == media)
            })
            .collect::<Vec<_>>();
        let clamped = self.timeline.clamp_media_duration(media, duration)?;
        let reconciled =
            self.timeline
                .reconcile_provisional_media_duration(media, &provisional, duration)?;
        let duration_changed = self
            .media
            .iter_mut()
            .find(|item| item.id == media_id)
            .is_some_and(|item| {
                let changed = item.duration != Some(duration);
                item.duration = Some(duration);
                changed
            });
        let unresolved_before = self.provisional_clip_ids.len();
        self.provisional_clip_ids
            .retain(|clip_id| !provisional.contains(clip_id));
        let resolved = self.provisional_clip_ids.len() != unresolved_before;
        let refit = self.auto_fit_provisional_view && (clamped > 0 || reconciled > 0 || resolved);
        if refit {
            self.apply_full_extent_view();
            self.auto_fit_provisional_view = !self.provisional_clip_ids.is_empty();
        }
        if clamped > 0 || reconciled > 0 || duration_changed || resolved || refit {
            self.mark_durable_edit();
        }
        Ok(())
    }

    /// Keeps a prior texture visible until a replacement is ready, avoiding monitor flicker.
    pub fn set_monitor_frame(&mut self, texture: egui::TextureId, width: u32, height: u32) {
        self.set_monitor_frame_for_source(texture, width, height, None, None);
    }

    /// Installs the newest frame published by the live decoder.
    pub fn set_monitor_frame_for_source(
        &mut self,
        texture: egui::TextureId,
        width: u32,
        height: u32,
        media_id: Option<MediaId>,
        source_tick: Option<Tick>,
    ) {
        self.set_monitor_frame_for_layer(0, texture, width, height, media_id, source_tick);
    }

    /// Returns the retained decoded frame for one viewer layer, if it is ready.
    pub fn monitor_frame_for_layer(&self, layer: usize) -> Option<MonitorFrame> {
        self.monitor_layers.get(layer).copied().flatten()
    }

    /// Returns runtime decode evidence for one bottom-to-top preview layer.
    pub fn active_preview_diagnostic_for_layer(
        &self,
        layer: usize,
    ) -> Option<ActivePreviewDiagnostic> {
        self.active_preview_diagnostics
            .get(layer)
            .copied()
            .flatten()
    }

    /// Records runtime decode evidence for one bottom-to-top preview layer.
    ///
    /// Invalid layer indexes are ignored and return `false`, keeping completion notifications
    /// from a stale decoder harmless on the UI thread.
    pub fn set_active_preview_diagnostic_for_layer(
        &mut self,
        layer: usize,
        diagnostic: ActivePreviewDiagnostic,
    ) -> bool {
        let Some(slot) = self.active_preview_diagnostics.get_mut(layer) else {
            return false;
        };
        *slot = Some(diagnostic);
        true
    }

    /// Clears runtime decode evidence for one preview layer.
    pub fn clear_active_preview_diagnostic_for_layer(&mut self, layer: usize) -> bool {
        let Some(slot) = self.active_preview_diagnostics.get_mut(layer) else {
            return false;
        };
        *slot = None;
        true
    }

    /// Clears all runtime preview decode evidence without affecting project persistence.
    pub fn clear_active_preview_diagnostics(&mut self) {
        self.active_preview_diagnostics = [None; PREVIEW_VIDEO_LAYER_COUNT];
    }

    /// Installs the newest decoded frame for one bottom-to-top viewer layer.
    ///
    /// Invalid layer indexes are ignored so decode completion cannot panic the UI thread.
    pub fn set_monitor_frame_for_layer(
        &mut self,
        layer: usize,
        texture: egui::TextureId,
        width: u32,
        height: u32,
        media_id: Option<MediaId>,
        source_tick: Option<Tick>,
    ) {
        let frame = MonitorFrame {
            texture,
            width: width.max(1),
            height: height.max(1),
            media_id,
            source_tick,
        };
        let Some(slot) = self.monitor_layers.get_mut(layer) else {
            return;
        };
        *slot = Some(frame);
        // The legacy singular API exposes the most recently supplied ready frame.
        self.monitor = Some(frame);
        self.monitor_status = MonitorStatus::Ready;
    }

    pub fn set_monitor_error(&mut self, error: impl Into<String>) {
        self.monitor_status = MonitorStatus::Error(error.into());
    }

    pub fn reset_monitor(&mut self) {
        self.monitor_layers = [None; PREVIEW_VIDEO_LAYER_COUNT];
        self.clear_active_preview_diagnostics();
        self.monitor = None;
        self.monitor_status = MonitorStatus::Empty;
    }

    /// Drops one retained layer while allowing another ready viewer layer to remain visible.
    pub fn reset_monitor_layer(&mut self, layer: usize) {
        let Some(slot) = self.monitor_layers.get_mut(layer) else {
            return;
        };
        *slot = None;
        let _ = self.clear_active_preview_diagnostic_for_layer(layer);
        self.monitor = self.monitor_layers.iter().rev().flatten().copied().next();
        self.monitor_status = if self.monitor.is_some() {
            MonitorStatus::Ready
        } else {
            MonitorStatus::Empty
        };
    }

    /// A gap has no valid source frame, so the monitor returns to black immediately.
    fn clear_monitor_for_gap(&mut self) {
        self.reset_monitor();
    }

    fn clear_stale_monitor_for_scrub_gap(&mut self) {
        if self.playback_target().is_none() {
            self.clear_monitor_for_gap();
        }
    }

    /// The persisted user preference. `Auto` is resolved by the runtime decode policy.
    pub const fn preview_quality(&self) -> PreviewQuality {
        self.preview_quality
    }

    /// The concrete quality currently used for monitor decoding. This is never `Auto`.
    pub const fn resolved_preview_quality(&self) -> PreviewQuality {
        match self.preview_quality {
            PreviewQuality::Auto => self.auto_preview_quality,
            quality => quality,
        }
    }

    /// The persisted preference used when the viewer is paused.
    pub const fn paused_preview_quality(&self) -> PreviewQuality {
        self.paused_preview_quality
    }

    /// The concrete paused-viewer quality. Legacy `Auto` values remain safe to restore.
    pub const fn resolved_paused_preview_quality(&self) -> PreviewQuality {
        match self.paused_preview_quality {
            PreviewQuality::Auto => self.auto_preview_quality,
            quality => quality,
        }
    }

    /// Selects the quality stored with the project. Retained monitor frames intentionally remain
    /// visible while replacements at the new resolution are decoded.
    pub fn set_preview_quality(&mut self, quality: PreviewQuality) -> bool {
        if self.preview_quality == quality {
            return false;
        }
        self.preview_quality = quality;
        self.mark_durable_edit();
        true
    }

    /// Selects the durable paused-viewer quality. `Auto` is accepted only for compatibility with
    /// older restored data; the UI intentionally offers concrete paused resolutions only.
    pub fn set_paused_preview_quality(&mut self, quality: PreviewQuality) -> bool {
        if self.paused_preview_quality == quality {
            return false;
        }
        self.paused_preview_quality = quality;
        self.mark_durable_edit();
        true
    }

    /// Whether playback uses the high-quality path.
    pub const fn high_quality_playback(&self) -> bool {
        self.high_quality_playback
    }

    /// Stores the high-quality playback preference.
    pub fn set_high_quality_playback(&mut self, enabled: bool) -> bool {
        if self.high_quality_playback == enabled {
            return false;
        }
        self.high_quality_playback = enabled;
        self.mark_durable_edit();
        true
    }

    /// Updates Auto's runtime decision without changing the project document.
    ///
    /// `Auto` is rejected because the resolved value must always be concrete.
    pub fn set_auto_preview_quality(&mut self, quality: PreviewQuality) -> bool {
        if quality == PreviewQuality::Auto || self.auto_preview_quality == quality {
            return false;
        }
        self.auto_preview_quality = quality;
        true
    }

    /// Quantized live-decode dimensions after applying the selected quality level.
    pub fn monitor_decode_size_hint(&self) -> (u32, u32) {
        self.monitor_playback_decode_size_hint()
    }

    /// Quantized moving-playback and scrub decode dimensions.
    pub fn monitor_playback_decode_size_hint(&self) -> (u32, u32) {
        scale_monitor_size(self.monitor_decode_size, self.resolved_preview_quality())
    }

    /// Quantized paused-viewer decode dimensions.
    pub fn monitor_paused_decode_size_hint(&self) -> (u32, u32) {
        scale_monitor_size(
            self.monitor_decode_size,
            self.resolved_paused_preview_quality(),
        )
    }

    /// Quantized live-decode dimensions for an active timeline scrub.
    ///
    /// Scrubbing uses the user's moving playback resolution exactly.
    pub fn monitor_scrub_decode_size_hint(&self) -> (u32, u32) {
        self.monitor_playback_decode_size_hint()
    }

    /// Project raster used by the preview compositor.  This is supplied by the project/output
    /// owner and intentionally stays outside the editor workspace snapshot.
    pub const fn project_canvas_size(&self) -> (u32, u32) {
        self.project_canvas_size
    }

    /// Updates the project raster when both dimensions are valid.  A false return leaves the
    /// previous canvas intact so a transient unset output format cannot collapse the viewer.
    pub fn set_project_canvas_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || self.project_canvas_size == (width, height) {
            return false;
        }
        self.project_canvas_size = (width, height);
        true
    }

    /// True only while the timeline ruler owns an active scrub gesture.
    pub fn is_scrubbing(&self) -> bool {
        matches!(self.timeline_drag, Some(TimelineDrag::Scrub))
    }

    /// Resolves up to four visible video sources, in bottom-to-top visual order.
    ///
    /// A cross dissolve contributes its outgoing and incoming sources as one adjacent pair. A dip
    /// to black contributes only the side of the cut that is currently visible. The fixed-size
    /// result avoids allocating during pointer-driven playback updates; when the bounded
    /// compositor is full, the same topmost-source policy used for stacked tracks wins.
    pub fn playback_targets(&self) -> impl Iterator<Item = PlaybackTarget<'_>> + '_ {
        let timeline_end = self.timeline_end();
        let mut targets = [None; PREVIEW_VIDEO_LAYER_COUNT];
        let mut insertion = PREVIEW_VIDEO_LAYER_COUNT;
        for track in self.timeline.tracks.iter().rev() {
            if track.kind != TrackKind::Video || track.muted {
                continue;
            }
            let track_targets = self.playback_targets_for_track(track, timeline_end);
            let source_count = track_targets.iter().flatten().count();
            if source_count > insertion {
                // A two-source transition is one visual track operation. Never admit only its
                // incoming or outgoing half when higher tracks consume the bounded layer budget.
                continue;
            }
            for target in track_targets.into_iter().rev().flatten() {
                if insertion == 0 {
                    break;
                }
                insertion -= 1;
                targets[insertion] = Some(target);
            }
            if insertion == 0 {
                break;
            }
        }
        targets.into_iter().flatten()
    }

    /// Compatibility helper for callers that intentionally need only the foremost layer.
    pub fn playback_target(&self) -> Option<PlaybackTarget<'_>> {
        self.playback_targets().last()
    }

    fn playback_targets_for_track(
        &self,
        track: &Track,
        timeline_end: Tick,
    ) -> [Option<PlaybackTarget<'_>>; 2] {
        if let Some(transition) = self.timeline.transitions().iter().find(|transition| {
            transition.track_id == track.id
                && [transition.left_clip, transition.right_clip]
                    .into_iter()
                    .all(|clip_id| self.timeline.clip(clip_id).is_some_and(|clip| clip.enabled))
                && self
                    .timeline
                    .transition_timing(transition.id)
                    .is_some_and(|(start, end)| self.playhead >= start && self.playhead < end)
        }) && let Some(progress) = self
            .timeline
            .transition_progress(transition.id, self.playhead)
        {
            match transition.kind {
                VideoTransitionKind::CrossDissolve | VideoTransitionKind::FilmDissolve => {
                    let incoming_mix =
                        shaped_transition_progress(transition.duration, transition.curve, progress);
                    let incoming_mix = if transition.kind == VideoTransitionKind::FilmDissolve {
                        // Match export's brighter gamma-shaped film response.
                        incoming_mix.powf(0.65)
                    } else {
                        incoming_mix
                    };
                    let outgoing = self
                        .timeline
                        .clip(transition.left_clip)
                        .and_then(|clip| self.playback_target_for_clip(clip, 1.0, 0.0, 0.0));
                    let incoming = self.timeline.clip(transition.right_clip).and_then(|clip| {
                        self.playback_target_for_clip(clip, incoming_mix, 0.0, 0.0)
                    });
                    if outgoing.is_some() || incoming.is_some() {
                        return [outgoing, incoming];
                    }
                }
                VideoTransitionKind::DipToBlack | VideoTransitionKind::DipToWhite => {
                    let cut = self.timeline.clip(transition.left_clip).map(Clip::end);
                    let target = cut.and_then(|cut| {
                        if self.playhead < cut {
                            let half = transition.duration.0 / 2;
                            let opacity = if half > 0 {
                                fade_envelope_value(
                                    Fade {
                                        duration: Tick(half),
                                        curve: transition.curve,
                                    },
                                    cut.0.saturating_sub(self.playhead.0) as f32 / half as f32,
                                )
                            } else {
                                0.0
                            };
                            self.timeline
                                .clip(transition.left_clip)
                                .and_then(|clip| {
                                    self.playback_target_for_clip(clip, 1.0, 0.0, 1.0 - opacity)
                                })
                                .map(|mut target| {
                                    if transition.kind == VideoTransitionKind::DipToWhite {
                                        target.black_matte_after = 0.0;
                                        target.white_matte_after = 1.0 - opacity;
                                    }
                                    target
                                })
                        } else {
                            let half = transition.duration.0 - transition.duration.0 / 2;
                            let opacity = if half > 0 {
                                fade_envelope_value(
                                    Fade {
                                        duration: Tick(half),
                                        curve: transition.curve,
                                    },
                                    self.playhead.0.saturating_sub(cut.0) as f32 / half as f32,
                                )
                            } else {
                                0.0
                            };
                            self.timeline
                                .clip(transition.right_clip)
                                .and_then(|clip| {
                                    self.playback_target_for_clip(clip, opacity, 1.0, 0.0)
                                })
                                .map(|mut target| {
                                    if transition.kind == VideoTransitionKind::DipToWhite {
                                        target.black_matte_before = 0.0;
                                        target.white_matte_before = 1.0;
                                    }
                                    target
                                })
                        }
                    });
                    if target.is_some() {
                        return [target, None];
                    }
                }
                VideoTransitionKind::WipeLeft
                | VideoTransitionKind::WipeRight
                | VideoTransitionKind::WipeUp
                | VideoTransitionKind::WipeDown => {
                    let progress =
                        shaped_transition_progress(transition.duration, transition.curve, progress);
                    let reveal = match transition.kind {
                        VideoTransitionKind::WipeLeft => TransitionReveal::FromLeft,
                        VideoTransitionKind::WipeRight => TransitionReveal::FromRight,
                        VideoTransitionKind::WipeUp => TransitionReveal::FromTop,
                        VideoTransitionKind::WipeDown => TransitionReveal::FromBottom,
                        _ => unreachable!(),
                    };
                    let outgoing = self
                        .timeline
                        .clip(transition.left_clip)
                        .and_then(|clip| self.playback_target_for_clip(clip, 1.0, 0.0, 0.0));
                    let incoming = self.timeline.clip(transition.right_clip).and_then(|clip| {
                        self.playback_target_for_clip(clip, 1.0, 0.0, 0.0)
                            .map(|mut target| {
                                target.transition_reveal = Some(reveal);
                                target.transition_offset = (progress.clamp(0.0, 1.0), 0.0);
                                target
                            })
                    });
                    if outgoing.is_some() || incoming.is_some() {
                        return [outgoing, incoming];
                    }
                }
                VideoTransitionKind::SlideFromLeft
                | VideoTransitionKind::SlideFromRight
                | VideoTransitionKind::SlideFromTop
                | VideoTransitionKind::SlideFromBottom => {
                    let progress =
                        shaped_transition_progress(transition.duration, transition.curve, progress);
                    let offset = match transition.kind {
                        VideoTransitionKind::SlideFromLeft => (progress - 1.0, 0.0),
                        VideoTransitionKind::SlideFromRight => (1.0 - progress, 0.0),
                        VideoTransitionKind::SlideFromTop => (0.0, progress - 1.0),
                        VideoTransitionKind::SlideFromBottom => (0.0, 1.0 - progress),
                        _ => unreachable!(),
                    };
                    let outgoing = self
                        .timeline
                        .clip(transition.left_clip)
                        .and_then(|clip| self.playback_target_for_clip(clip, 1.0, 0.0, 0.0));
                    let incoming = self.timeline.clip(transition.right_clip).and_then(|clip| {
                        self.playback_target_for_clip(clip, 1.0, 0.0, 0.0)
                            .map(|mut target| {
                                target.transition_offset = offset;
                                target
                            })
                    });
                    if outgoing.is_some() || incoming.is_some() {
                        return [outgoing, incoming];
                    }
                }
            }
        }

        let first_not_ended = track
            .clips
            .partition_point(|clip| clip.end() <= self.playhead);
        let Some(clip) = track.clips.get(first_not_ended).or_else(|| {
            (self.playhead == timeline_end)
                .then(|| track.clips.last())
                .flatten()
        }) else {
            return [None, None];
        };
        let clip_end = clip.end();
        let covers_playhead = clip.start <= self.playhead
            && (self.playhead < clip_end
                || (self.playhead == timeline_end && clip_end == timeline_end));
        [
            covers_playhead
                .then(|| self.playback_target_for_clip(clip, 1.0, 0.0, 0.0))
                .flatten(),
            None,
        ]
    }

    fn playback_target_for_clip(
        &self,
        clip: &Clip,
        transition_opacity: f32,
        black_matte_before: f32,
        black_matte_after: f32,
    ) -> Option<PlaybackTarget<'_>> {
        if !clip.enabled {
            return None;
        }
        let media_id = clip.media.0;
        let item = self.media.get(media_id.saturating_sub(1) as usize)?;
        let path = (item.id == media_id).then_some(&item.path)?;
        let clip_end = clip.end();
        let source_out = Tick(clip.source_in.0.saturating_add(clip.duration.0));
        let indexed_source_frames = (item.kind != MediaKind::Image)
            .then(|| self.source_frame_time_indexes.get(&media_id))
            .flatten();
        let source_tick = if self.playhead == self.timeline_end() && self.playhead == clip_end {
            let last_source_microtick = Tick(source_out.0.saturating_sub(1));
            if indexed_source_frames
                .and_then(|index| index.resolve(last_source_microtick))
                .is_some()
            {
                last_source_microtick
            } else {
                self.frame_rate.frame_before_end(source_out)
            }
        } else {
            Tick(
                clip.source_in
                    .0
                    .saturating_add(self.playhead.0.saturating_sub(clip.start.0)),
            )
        };
        let indexed_frame = indexed_source_frames.and_then(|index| index.resolve(source_tick));
        let decode_tick = if item.kind == MediaKind::Image {
            Tick(0)
        } else {
            indexed_frame.map_or(source_tick, |(tick, _)| tick)
        };
        let envelope_tick = Tick(
            self.playhead
                .0
                .saturating_sub(clip.start.0)
                .clamp(0, clip.duration.0),
        );
        Some(PlaybackTarget {
            clip_id: clip.id,
            media_id,
            path,
            source_tick,
            decode_tick,
            source_frame_rate: indexed_frame
                .is_none()
                .then(|| {
                    self.media_metadata
                        .get(&media_id)
                        .and_then(|metadata| metadata.frame_rate_ratio)
                        .filter(|_| item.kind != MediaKind::Image)
                })
                .flatten(),
            source_frame_duration_tick: indexed_frame.and_then(|(_, duration)| duration),
            opacity: video_opacity_at(clip, envelope_tick) * transition_opacity.clamp(0.0, 1.0),
            black_matte_before: black_matte_before.clamp(0.0, 1.0),
            black_matte_after: black_matte_after.clamp(0.0, 1.0),
            white_matte_before: 0.0,
            white_matte_after: 0.0,
            transition_reveal: None,
            transition_offset: (0.0, 0.0),
            source_size: self.media_metadata.get(&media_id).and_then(|metadata| {
                match (metadata.width, metadata.height) {
                    (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
                    _ => None,
                }
            }),
            transform: clip.transform,
            video_effects: clip.evaluate_video_effects(source_tick),
        })
    }

    fn visit_resolved_audio_playback_sources<'a>(
        &'a self,
        mut visit: impl FnMut(ResolvedAudioPlaybackSource<'a>),
    ) {
        let any_solo = self
            .timeline
            .tracks
            .iter()
            .any(|track| matches!(track.kind, TrackKind::Audio) && track.solo);
        for track in self
            .timeline
            .tracks
            .iter()
            .filter(|track| track.audio_is_audible(any_solo))
        {
            let transition = self.timeline.audio_transitions().iter().find(|transition| {
                transition.track_id == track.id
                    && [transition.left_clip, transition.right_clip]
                        .into_iter()
                        .all(|clip_id| self.timeline.clip(clip_id).is_some_and(|clip| clip.enabled))
                    && self
                        .timeline
                        .audio_transition_timing(transition.id)
                        .is_some_and(|(start, end)| self.playhead >= start && self.playhead < end)
            });
            if let Some(transition) = transition
                && let Some((_window_start, _)) =
                    self.timeline.audio_transition_timing(transition.id)
                && let (Some(outgoing), Some(incoming)) = (
                    self.timeline.clip(transition.left_clip),
                    self.timeline.clip(transition.right_clip),
                )
            {
                let duration = transition.duration;
                let left_half = duration.0 / 2;
                for (clip, role) in [
                    (outgoing, AudioPlaybackTransitionRole::Outgoing),
                    (incoming, AudioPlaybackTransitionRole::Incoming),
                ] {
                    let Some(item) = self.media.get(clip.media.0.saturating_sub(1) as usize) else {
                        continue;
                    };
                    if item.id != clip.media.0 {
                        continue;
                    }
                    let clip_tick = Tick(self.playhead.0 - clip.start.0);
                    let Some(source_tick) = clip
                        .source_in
                        .0
                        .checked_add(clip_tick.0)
                        .filter(|tick| *tick >= 0)
                        .map(Tick)
                    else {
                        continue;
                    };
                    visit(ResolvedAudioPlaybackSource {
                        track,
                        clip,
                        path: &item.path,
                        source: AudioPlaybackSource {
                            track_id: track.id,
                            clip_id: clip.id,
                            media_id: clip.media.0,
                            source_tick,
                            clip_tick,
                            transition: Some(AudioPlaybackTransitionEnvelope {
                                role,
                                start_clip_tick: match role {
                                    AudioPlaybackTransitionRole::Outgoing => {
                                        Tick(clip.duration.0 - left_half)
                                    }
                                    AudioPlaybackTransitionRole::Incoming => Tick(-left_half),
                                },
                                duration_ticks: duration,
                            }),
                        },
                    });
                }
                continue;
            }
            let Some(target) = (|| {
                let first_not_ended = track
                    .clips
                    .partition_point(|clip| clip.end() <= self.playhead);
                let clip = track.clips.get(first_not_ended).filter(|clip| {
                    clip.enabled && clip.start <= self.playhead && self.playhead < clip.end()
                })?;
                let item = self.media.get(clip.media.0.saturating_sub(1) as usize)?;
                let path = (item.id == clip.media.0).then_some(&item.path)?;
                Some(ResolvedAudioPlaybackSource {
                    track,
                    clip,
                    path,
                    source: AudioPlaybackSource {
                        track_id: track.id,
                        clip_id: clip.id,
                        media_id: clip.media.0,
                        source_tick: Tick(clip.source_in.0 + self.playhead.0 - clip.start.0),
                        clip_tick: Tick(self.playhead.0 - clip.start.0),
                        transition: None,
                    },
                })
            })() else {
                continue;
            };
            visit(target);
        }
    }

    /// Visits lightweight audible source metadata without allocating effect stacks.
    pub fn visit_audio_playback_sources(&self, mut visit: impl FnMut(AudioPlaybackSource)) {
        self.visit_resolved_audio_playback_sources(|resolved| visit(resolved.source));
    }

    /// Collects audible targets for callers that need to retain them.
    pub fn audio_playback_targets(&self) -> Vec<AudioPlaybackTarget<'_>> {
        let mut targets = Vec::new();
        self.visit_resolved_audio_playback_sources(|resolved| {
            let source = resolved.source;
            targets.push(AudioPlaybackTarget {
                track_id: source.track_id,
                clip_id: source.clip_id,
                media_id: source.media_id,
                path: resolved.path,
                source_tick: source.source_tick,
                clip_tick: source.clip_tick,
                gain_db: resolved.clip.mix_gain_db(resolved.track),
                gain_left_db: resolved.clip.gain_left_db,
                gain_right_db: resolved.clip.gain_right_db,
                pan: resolved.track.pan,
                effects: enabled_audio_effects(resolved.clip, resolved.track),
                fade_in_ticks: resolved.clip.fade_in.duration,
                fade_in_curve: resolved.clip.fade_in.curve,
                fade_out_ticks: resolved.clip.fade_out.duration,
                fade_out_curve: resolved.clip.fade_out.curve,
                clip_duration: resolved.clip.duration,
                transition: source.transition,
            });
        });
        targets
    }

    /// Compatibility helper for callers that intentionally need one audible lane.
    pub fn audio_playback_target(&self) -> Option<AudioPlaybackTarget<'_>> {
        self.audio_playback_targets().into_iter().last()
    }

    pub fn timeline_end(&self) -> Tick {
        let clip_end = self
            .timeline
            .tracks
            .iter()
            .filter_map(|track| track.clips.last())
            .map(nle_timeline::Clip::end)
            .max()
            .unwrap_or(Tick(0));
        let title_end = self
            .timeline
            .titles()
            .iter()
            .map(|title| Tick(title.start.0.saturating_add(title.duration.0)))
            .max()
            .unwrap_or(Tick(0));
        clip_end.max(title_end)
    }

    pub fn set_playhead(&mut self, tick: Tick) {
        self.set_playhead_inner(tick, true);
    }

    fn set_playhead_inner(&mut self, tick: Tick, durable: bool) {
        let previous = self.playhead;
        self.playhead = Tick(tick.0.clamp(0, self.timeline_end().0));
        if self.playhead >= self.timeline_end() {
            self.playing = false;
        }
        if durable && self.playhead != previous {
            self.mark_durable_edit();
        }
    }

    pub fn start_playback(&mut self) {
        if self.playhead < self.timeline_end() {
            self.playing = true;
        }
    }

    pub fn toggle_playback(&mut self) {
        if self.playing {
            self.playing = false;
        } else {
            self.start_playback();
        }
    }

    pub fn previous_frame(&mut self) {
        let previous = self
            .frame_rate
            .frame_index_at_tick(Tick(self.playhead.0.saturating_sub(1)));
        self.set_playhead(self.frame_rate.frame_boundary_tick(previous));
    }

    pub fn next_frame(&mut self) {
        let next = self
            .frame_rate
            .frame_index_at_tick(self.playhead)
            .saturating_add(1);
        self.set_playhead(self.frame_rate.frame_boundary_tick(next));
    }

    fn dynamic_trim_delta(&self, clip_id: ClipId, direction: i64) -> Option<Tick> {
        let clip = self.timeline.clip(clip_id)?;
        let source_end = Tick(clip.source_in.0.saturating_add(clip.duration.0));
        let target = if direction.is_negative() {
            self.frame_rate.frame_boundary_tick(
                self.frame_rate
                    .frame_index_at_tick(Tick(source_end.0.saturating_sub(1))),
            )
        } else {
            self.frame_rate.frame_boundary_tick(
                self.frame_rate
                    .frame_index_at_tick(source_end)
                    .saturating_add(1),
            )
        };
        Some(Tick(target.0.saturating_sub(source_end.0)))
    }

    /// Advances the logical playback clock. It never waits on monitor decode latency.
    pub fn advance_playback(&mut self, elapsed: Duration) {
        if !self.playing {
            return;
        }
        let micros = elapsed.as_micros().min(i64::MAX as u128) as i64;
        self.set_playhead_inner(Tick(self.playhead.0.saturating_add(micros)), false);
    }

    /// HOT PATH — no IO. Locks the visual transport to an external native audio-device clock.
    /// This is runtime-only and deliberately does not dirty the project document.
    pub fn synchronize_playback_clock(&mut self, tick: Tick) {
        if self.playing {
            self.set_playhead_inner(tick, false);
        }
    }

    fn update_monitor_decode_size(&mut self, bounds: Vec2, pixels_per_point: f32) {
        self.monitor_decode_size = quantize_monitor_size(bounds.x, bounds.y, pixels_per_point);
    }

    pub fn cached_waveform(&self, media_id: MediaId) -> Option<&CachedWaveform> {
        self.waveforms.get(&media_id).map(Arc::as_ref)
    }

    fn rebuild_timeline_media_draw_slots_if_stale(&mut self) {
        if !self.timeline_media_draw_slots_dirty {
            return;
        }
        self.timeline_media_draw_slots.clear();
        self.timeline_media_draw_slots
            .resize_with(self.media.len(), TimelineMediaDrawSlot::default);
        for (index, item) in self.media.iter().enumerate() {
            let slot = &mut self.timeline_media_draw_slots[index];
            slot.waveform = self.waveforms.get(&item.id).cloned();
            slot.waveform_failed = self.waveform_errors.contains_key(&item.id);
            slot.offline = self.media_errors.contains_key(&item.id);
            slot.video_strip = self.video_strips.get(&item.id).copied();
        }
        for flag in &self.flags {
            if let Some(slot) = flag
                .media_id
                .checked_sub(1)
                .and_then(|index| self.timeline_media_draw_slots.get_mut(index as usize))
            {
                slot.flag_color = Some(marker_color(flag.color));
            }
        }
        self.timeline_media_draw_slots_dirty = false;
    }

    /// Places selected media at the end of its default target track. Video starts on V1/A1,
    /// audio on A1, and still images on V1 without a linked audio clip.
    pub fn add_selected_to_timeline(&mut self) -> bool {
        let Some(item) = self.selected() else {
            return false;
        };
        let next_start = |kind: TrackKind| -> Tick {
            self.timeline
                .tracks
                .iter()
                .find(|track| track.kind == kind)
                .and_then(|track| track.clips.last())
                .map(|clip| clip.end())
                .unwrap_or(Tick(0))
        };
        let start = match item.kind {
            MediaKind::Video => next_start(TrackKind::Video).max(next_start(TrackKind::Audio)),
            MediaKind::Audio => next_start(TrackKind::Audio),
            MediaKind::Image => next_start(TrackKind::Video),
            MediaKind::Unknown => return false,
        };
        self.insert_media_at(item.id, start)
    }

    /// Inserts supported media on V1/A1 or A1 at an explicit timeline tick.
    /// It returns false without emitting work when a default track is unavailable
    /// or the requested placement overlaps an existing clip.
    pub fn insert_media_at(&mut self, media_id: MediaId, start: Tick) -> bool {
        let Some(item) = self.media.iter().find(|item| item.id == media_id).cloned() else {
            return false;
        };
        let media = TimelineMediaId(item.id);
        let (duration, provisional) = self.media_placement_duration(media_id);
        let before = self.timeline_history_checkpoint();
        let inserted = match item.kind {
            MediaKind::Video => self
                .timeline
                .insert_linked_av_pair(media, start, duration, Tick(0))
                .map(|pair| vec![pair.video, pair.audio]),
            MediaKind::Audio => self
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Audio)
                .map(|track| track.id)
                .ok_or(TimelineError::NoTrackOfKind(TrackKind::Audio))
                .and_then(|track| {
                    self.timeline
                        .insert_clip(track, media, start, duration, Tick(0))
                        .map(|clip| vec![clip])
                }),
            MediaKind::Image => self
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .map(|track| track.id)
                .ok_or(TimelineError::NoTrackOfKind(TrackKind::Video))
                .and_then(|track| {
                    self.timeline
                        .insert_clip(track, media, start, duration, Tick(0))
                        .map(|clip| vec![clip])
                }),
            MediaKind::Unknown => return false,
        };
        let Ok(inserted) = inserted else { return false };
        self.finish_media_placement(item, before, inserted, provisional)
    }

    /// Drops supported media onto V1/A1 or A1 using non-ripple overwrite semantics. Occupied
    /// sections become source-accurate left/right tails; unrelated later clips never move.
    pub fn overwrite_media_at(&mut self, media_id: MediaId, start: Tick) -> bool {
        let Some(item) = self.media.iter().find(|item| item.id == media_id).cloned() else {
            return false;
        };
        let (duration, provisional) = self.media_placement_duration(media_id);
        let before = self.timeline_history_checkpoint();
        let target = match item.kind {
            MediaKind::Video => EditTarget::VideoAndAudio,
            MediaKind::Audio => EditTarget::AudioOnly,
            MediaKind::Image => EditTarget::VideoOnly,
            MediaKind::Unknown => return false,
        };
        let Ok(inserted) = self.timeline.overwrite_edit(
            target,
            TimelineMediaId(media_id),
            start,
            duration,
            Tick(0),
        ) else {
            return false;
        };
        if inserted.is_empty() {
            return false;
        }
        self.finish_media_placement(item, before, inserted, provisional)
    }

    fn media_placement_duration(&self, media_id: MediaId) -> (Tick, bool) {
        if self
            .media
            .iter()
            .find(|item| item.id == media_id)
            .is_some_and(|item| item.kind == MediaKind::Image)
        {
            return (DEFAULT_STILL_IMAGE_DURATION, false);
        }
        let duration = self
            .media
            .iter()
            .find(|item| item.id == media_id)
            .and_then(|item| item.duration)
            .or_else(|| {
                self.waveforms
                    .get(&media_id)
                    .map(|waveform| waveform.duration)
            })
            .or_else(|| {
                self.video_strips
                    .get(&media_id)
                    .map(|strip| strip.layout.duration)
            })
            .filter(|duration| duration.0 > 0);
        duration.map_or((PROVISIONAL_MEDIA_DURATION, true), |duration| {
            (duration, false)
        })
    }

    fn finish_media_placement(
        &mut self,
        item: MediaItem,
        before: EditorHistoryCheckpoint,
        inserted: Vec<ClipId>,
        provisional: bool,
    ) -> bool {
        let Some(clip) = inserted.first().copied() else {
            return false;
        };
        let was_empty = before
            .timeline
            .tracks
            .iter()
            .all(|track| track.clips.is_empty());
        let retained_auto_fit = self.auto_fit_provisional_view;
        self.abandon_changed_provisional_since(&before.timeline);
        self.selected_timeline_clip = Some(clip);
        self.selected_title = None;
        if provisional {
            self.provisional_clip_ids.extend(inserted);
            self.auto_fit_provisional_view |= retained_auto_fit;
        }
        if was_empty {
            // An empty project has no meaningful time extent. Fit the first real bar immediately
            // so thumbnails, waveforms, fades, and dragging are usable instead of rendering a
            // short source against the legacy 4:12 empty-project range.
            self.apply_full_extent_view();
            self.auto_fit_provisional_view = provisional;
        }
        self.record_timeline_history(before);
        self.mark_durable_edit();
        self.emit(EditorAction::AnalyzeMedia {
            media_id: item.id,
            path: item.path,
        });
        true
    }

    pub fn take_action(&mut self) -> Option<EditorAction> {
        self.action.take()
    }

    pub fn set_zoom_handles(&mut self, left: f32, right: f32) {
        let previous = (
            self.zoom_left,
            self.zoom_right,
            self.timeline_view_start,
            self.timeline_view_span,
        );
        const MIN_GAP: f32 = 0.01;
        let left = left.clamp(0.0, 1.0 - MIN_GAP);
        let right = right.clamp(MIN_GAP, 1.0);
        if right - left < MIN_GAP {
            if left > self.zoom_left {
                self.zoom_left = (right - MIN_GAP).max(0.0);
                self.zoom_right = right;
            } else {
                self.zoom_left = left;
                self.zoom_right = (left + MIN_GAP).min(1.0);
            }
        } else {
            self.zoom_left = left;
            self.zoom_right = right;
        }
        self.set_custom_timeline_view();
        if previous
            != (
                self.zoom_left,
                self.zoom_right,
                self.timeline_view_start,
                self.timeline_view_span,
            )
        {
            self.mark_durable_edit();
        }
    }

    fn set_track_density(&mut self, density: TimelineTrackDensity) {
        if self.track_density == density
            && self
                .timeline
                .tracks
                .iter()
                .all(|track| self.track_heights.contains_key(&track.id))
        {
            return;
        }
        self.track_density = density;
        let height = match density {
            TimelineTrackDensity::Compact => 32.0,
            TimelineTrackDensity::Normal => 64.0,
            TimelineTrackDensity::Large => 120.0,
        };
        for track in &self.timeline.tracks {
            self.track_heights.insert(track.id, height);
        }
        self.mark_durable_edit();
    }

    fn reset_workspace_layout(&mut self) {
        let track_heights_changed = self.track_heights.len() != self.timeline.tracks.len()
            || self.timeline.tracks.iter().any(|track| {
                self.track_heights
                    .get(&track.id)
                    .is_none_or(|height| *height != 64.0)
            });
        let changed = self.media_pool_width != DEFAULT_MEDIA_POOL_WIDTH
            || self.analysis_width != DEFAULT_RIGHT_SIDEBAR_WIDTH
            || self.undertow_tools_width != default_undertow_tools_width()
            || self.undertow_mixer_width != default_undertow_mixer_width()
            || self.timeline_height != DEFAULT_TIMELINE_HEIGHT
            || !self.timeline_height_is_default
            || self.timeline_scroll_y != 0.0
            || self.track_density != TimelineTrackDensity::Normal
            || track_heights_changed;
        self.media_pool_width = DEFAULT_MEDIA_POOL_WIDTH;
        self.analysis_width = DEFAULT_RIGHT_SIDEBAR_WIDTH;
        self.undertow_tools_width = 190.0;
        self.undertow_mixer_width = 220.0;
        self.timeline_height = DEFAULT_TIMELINE_HEIGHT;
        self.timeline_height_is_default = true;
        self.timeline_scroll_y = 0.0;
        self.track_density = TimelineTrackDensity::Normal;
        self.track_heights.clear();
        for track in &self.timeline.tracks {
            self.track_heights.insert(track.id, 64.0);
        }
        if changed {
            self.mark_durable_edit();
        }
    }

    fn add_marker(&mut self, color: u8) {
        self.markers.push(TimelineMarker {
            tick: self.playhead,
            color: color % MARKER_COLORS.len() as u8,
        });
        self.mark_durable_edit();
    }

    fn clear_markers_at_playhead(&mut self) {
        let previous = self.markers.len();
        self.markers.retain(|marker| marker.tick != self.playhead);
        if self.markers.len() != previous {
            self.mark_durable_edit();
        }
    }

    fn set_selected_flag(&mut self, color: u8) {
        let Some(clip_id) = self.selected_timeline_clip else {
            return;
        };
        let Some(media_id) = self.timeline.clip(clip_id).map(|clip| clip.media.0) else {
            return;
        };
        self.flags.retain(|flag| flag.media_id != media_id);
        self.flags.push(TimelineFlag {
            media_id,
            color: color % MARKER_COLORS.len() as u8,
            legacy_clip_id: None,
        });
        self.timeline_media_draw_slots_dirty = true;
        self.mark_durable_edit();
    }

    fn clear_selected_flag(&mut self) {
        if let Some(media_id) = self
            .selected_timeline_clip
            .and_then(|clip_id| self.timeline.clip(clip_id))
            .map(|clip| clip.media.0)
        {
            let previous = self.flags.len();
            self.flags.retain(|flag| flag.media_id != media_id);
            if self.flags.len() != previous {
                self.timeline_media_draw_slots_dirty = true;
                self.mark_durable_edit();
            }
        }
    }

    fn add_timeline_track(&mut self, kind: TrackKind) {
        let before = self.timeline_history_checkpoint();
        self.timeline.add_track(kind);
        self.record_timeline_history(before);
        self.mark_durable_edit();
    }

    fn add_title_at_playhead(&mut self) -> bool {
        let before = self.timeline_history_checkpoint();
        let duration = Tick(5_000_000);
        let Ok(id) = self.timeline.add_title(self.playhead, duration, "Title") else {
            return false;
        };
        self.record_timeline_history(before);
        self.selected_timeline_clip = None;
        self.selected_title = Some(id);
        self.mark_durable_edit();
        true
    }

    fn transition_at_cut(&self, left_clip: ClipId, right_clip: ClipId) -> Option<&VideoTransition> {
        self.timeline.transitions().iter().find(|transition| {
            transition.left_clip == left_clip && transition.right_clip == right_clip
        })
    }

    fn adjacent_video_cut(
        &self,
        clip_id: ClipId,
        edge: FadeEdge,
    ) -> Option<(TrackId, ClipId, ClipId)> {
        let clip = self.timeline.clip(clip_id)?;
        let track = self.timeline.track(clip.track_id)?;
        if track.kind != TrackKind::Video {
            return None;
        }
        let index = track
            .clips
            .iter()
            .position(|candidate| candidate.id == clip_id)?;
        let (left, right) = match edge {
            FadeEdge::In if index > 0 => (&track.clips[index - 1], &track.clips[index]),
            FadeEdge::Out if index + 1 < track.clips.len() => {
                (&track.clips[index], &track.clips[index + 1])
            }
            _ => return None,
        };
        (left.end() == right.start).then_some((track.id, left.id, right.id))
    }

    fn adjacent_audio_cut(
        &self,
        clip_id: ClipId,
        edge: FadeEdge,
    ) -> Option<(TrackId, ClipId, ClipId)> {
        let clip = self.timeline.clip(clip_id)?;
        let track = self.timeline.track(clip.track_id)?;
        if track.kind != TrackKind::Audio {
            return None;
        }
        let index = track
            .clips
            .iter()
            .position(|candidate| candidate.id == clip_id)?;
        let (left, right) = match edge {
            FadeEdge::In if index > 0 => (&track.clips[index - 1], &track.clips[index]),
            FadeEdge::Out if index + 1 < track.clips.len() => {
                (&track.clips[index], &track.clips[index + 1])
            }
            _ => return None,
        };
        (left.end() == right.start).then_some((track.id, left.id, right.id))
    }

    fn can_add_video_transition(
        &self,
        clip_id: ClipId,
        edge: FadeEdge,
        kind: VideoTransitionKind,
    ) -> bool {
        self.adjacent_video_cut(clip_id, edge)
            .is_some_and(|(_, left, right)| {
                self.transition_at_cut(left, right).is_none()
                    && self
                        .transition_duration_capacity(left, right, kind, None)
                        .is_some_and(|capacity| {
                            capacity.0 >= self.frame_rate.frame_boundary_tick(1).0.max(1)
                        })
            })
    }

    fn toggle_audio_crossfade(&mut self, clip_id: ClipId, edge: FadeEdge) -> bool {
        let Some((track_id, left, right)) = self.adjacent_audio_cut(clip_id, edge) else {
            return false;
        };
        let before = self.timeline_history_checkpoint();
        if let Some(existing) = self
            .timeline
            .audio_transitions()
            .iter()
            .find(|transition| transition.left_clip == left && transition.right_clip == right)
            .map(|transition| transition.id)
        {
            if self.timeline.remove_audio_transition(existing).is_err() {
                return false;
            }
        } else {
            let Some(capacity) = self.audio_transition_duration_capacity(left, right, None) else {
                return false;
            };
            let duration = Tick(DEFAULT_VIDEO_TRANSITION_DURATION.0.min(capacity.0));
            if duration.0 <= 0
                || self
                    .timeline
                    .add_audio_transition(track_id, left, right, duration)
                    .is_err()
            {
                return false;
            }
        }
        self.record_timeline_history(before);
        self.mark_durable_edit();
        true
    }

    fn transition_handle_capacity(&self, left_clip: ClipId, right_clip: ClipId) -> Option<Tick> {
        let left = self.timeline.clip(left_clip)?;
        let right = self.timeline.clip(right_clip)?;
        let handle = |clip: &Clip, before: bool| -> Option<i64> {
            let item = self.media.get(clip.media.0.saturating_sub(1) as usize)?;
            if item.id != clip.media.0 {
                return None;
            }
            if item.kind == MediaKind::Image {
                return Some(i64::MAX / 4);
            }
            if before {
                Some(clip.source_in.0.max(0))
            } else {
                let media_duration = item.duration.or_else(|| {
                    self.media_metadata
                        .get(&item.id)
                        .and_then(|metadata| metadata.duration_seconds)
                        .and_then(duration_seconds_to_tick)
                })?;
                Some(
                    media_duration
                        .0
                        .saturating_sub(clip.source_in.0.saturating_add(clip.duration.0))
                        .max(0),
                )
            }
        };
        let capacity = left
            .duration
            .0
            .max(0)
            .saturating_mul(2)
            .saturating_add(1)
            .min(right.duration.0.max(0).saturating_mul(2))
            .min(handle(left, false)?.max(0).saturating_mul(2))
            .min(
                handle(right, true)?
                    .max(0)
                    .saturating_mul(2)
                    .saturating_add(1),
            );
        Some(Tick(capacity))
    }

    fn transition_duration_capacity(
        &self,
        left_clip: ClipId,
        right_clip: ClipId,
        kind: VideoTransitionKind,
        replacing: Option<TransitionId>,
    ) -> Option<Tick> {
        let left = self.timeline.clip(left_clip)?;
        let right = self.timeline.clip(right_clip)?;
        let incoming_overlap = self
            .timeline
            .transitions()
            .iter()
            .filter(|transition| {
                Some(transition.id) != replacing && transition.right_clip == left_clip
            })
            .map(|transition| transition.duration.0 - transition.duration.0 / 2)
            .sum::<i64>();
        let outgoing_overlap = self
            .timeline
            .transitions()
            .iter()
            .filter(|transition| {
                Some(transition.id) != replacing && transition.left_clip == right_clip
            })
            .map(|transition| transition.duration.0 / 2)
            .sum::<i64>();
        let left_capacity = left
            .duration
            .0
            .saturating_sub(incoming_overlap)
            .max(0)
            .saturating_mul(2)
            .saturating_add(1);
        let right_capacity = right
            .duration
            .0
            .saturating_sub(outgoing_overlap)
            .max(0)
            .saturating_mul(2);
        let shared_clip_capacity = left_capacity.min(right_capacity);
        let capacity = match kind {
            VideoTransitionKind::CrossDissolve
            | VideoTransitionKind::FilmDissolve
            | VideoTransitionKind::WipeLeft
            | VideoTransitionKind::WipeRight
            | VideoTransitionKind::WipeUp
            | VideoTransitionKind::WipeDown
            | VideoTransitionKind::SlideFromLeft
            | VideoTransitionKind::SlideFromRight
            | VideoTransitionKind::SlideFromTop
            | VideoTransitionKind::SlideFromBottom => self
                .transition_handle_capacity(left_clip, right_clip)?
                .0
                .min(shared_clip_capacity),
            VideoTransitionKind::DipToBlack | VideoTransitionKind::DipToWhite => {
                shared_clip_capacity
            }
        };
        Some(Tick(capacity))
    }

    fn audio_transition_duration_capacity(
        &self,
        left_clip: ClipId,
        right_clip: ClipId,
        replacing: Option<AudioTransitionId>,
    ) -> Option<Tick> {
        let left = self.timeline.clip(left_clip)?;
        let right = self.timeline.clip(right_clip)?;
        let incoming_overlap = self
            .timeline
            .audio_transitions()
            .iter()
            .filter(|transition| {
                Some(transition.id) != replacing && transition.right_clip == left_clip
            })
            .map(|transition| transition.duration.0 - transition.duration.0 / 2)
            .sum::<i64>();
        let outgoing_overlap = self
            .timeline
            .audio_transitions()
            .iter()
            .filter(|transition| {
                Some(transition.id) != replacing && transition.left_clip == right_clip
            })
            .map(|transition| transition.duration.0 / 2)
            .sum::<i64>();
        let shared_clip_capacity = left
            .duration
            .0
            .saturating_sub(incoming_overlap)
            .max(0)
            .saturating_mul(2)
            .saturating_add(1)
            .min(
                right
                    .duration
                    .0
                    .saturating_sub(outgoing_overlap)
                    .max(0)
                    .saturating_mul(2),
            );
        Some(Tick(
            self.transition_handle_capacity(left_clip, right_clip)?
                .0
                .min(shared_clip_capacity),
        ))
    }

    fn add_video_transition(&mut self, edge: FadeEdge, kind: VideoTransitionKind) -> bool {
        let Some(selected) = self.selected_timeline_clip else {
            return false;
        };
        let Some((track_id, left, right)) = self.adjacent_video_cut(selected, edge) else {
            return false;
        };
        if self.transition_at_cut(left, right).is_some() {
            return false;
        }
        let Some(capacity) = self.transition_duration_capacity(left, right, kind, None) else {
            return false;
        };
        let minimum = self.frame_rate.frame_boundary_tick(1).0.max(1);
        let duration = Tick(DEFAULT_VIDEO_TRANSITION_DURATION.0.min(capacity.0));
        if duration.0 < minimum {
            return false;
        }
        let before = self.timeline_history_checkpoint();
        if self
            .timeline
            .add_video_transition_of_kind(track_id, left, right, duration, 0.0, kind)
            .is_err()
        {
            return false;
        }
        self.record_timeline_history(before);
        self.mark_durable_edit();
        true
    }

    /// Resolves a catalog drop only at a real, visible video cut. Keeping this separate from
    /// pointer handling makes rejected drops side-effect free and keeps the model mutation at
    /// the same undo seam as the existing inspector controls.
    fn transition_drop_target_at(
        &self,
        point: Pos2,
        kind: VideoTransitionKind,
    ) -> Option<(TrackId, ClipId, ClipId)> {
        let geometry = self.timeline_drop_geometry?;
        if !geometry.content.contains(point) {
            return None;
        }
        let row = self
            .timeline_track_rows
            .iter()
            .find(|row| row.kind == TrackKind::Video && row.rect.contains(point))?;
        // A forgiving edit-point snap keeps drag/drop usable on high-DPI displays. If several
        // cuts are compressed into that window, always choose the one closest to the pointer.
        let tolerance = 20.0;
        let cut = self
            .timeline
            .track(row.track_id)?
            .clips
            .windows(2)
            .filter_map(|pair| {
                let (left, right) = (&pair[0], &pair[1]);
                if left.end() != right.start {
                    return None;
                }
                let cut_x = geometry.content.left()
                    + (left.end().0 - geometry.view_start.0) as f32
                        / geometry.visible_ticks.max(1.0)
                        * geometry.content.width();
                let distance = (point.x - cut_x).abs();
                (distance <= tolerance).then_some((distance, left.id, right.id))
            })
            .min_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, left, right)| (left, right))?;
        if self.transition_at_cut(cut.0, cut.1).is_some() {
            return None;
        }
        let minimum = self.frame_rate.frame_boundary_tick(1).0.max(1);
        self.transition_duration_capacity(cut.0, cut.1, kind, None)
            .filter(|capacity| capacity.0 >= minimum)
            .map(|_| (row.track_id, cut.0, cut.1))
    }

    fn add_video_transition_at_cut(
        &mut self,
        track_id: TrackId,
        left: ClipId,
        right: ClipId,
        kind: VideoTransitionKind,
    ) -> bool {
        if self.transition_at_cut(left, right).is_some() {
            return false;
        }
        let minimum = self.frame_rate.frame_boundary_tick(1).0.max(1);
        let Some(capacity) = self.transition_duration_capacity(left, right, kind, None) else {
            return false;
        };
        let duration = Tick(DEFAULT_VIDEO_TRANSITION_DURATION.0.min(capacity.0));
        if duration.0 < minimum {
            return false;
        }
        let before = self.timeline_history_checkpoint();
        if self
            .timeline
            .add_video_transition_of_kind(track_id, left, right, duration, 0.0, kind)
            .is_err()
        {
            return false;
        }
        self.record_timeline_history(before);
        self.selected_timeline_clip = Some(left);
        self.selected_title = None;
        self.right_sidebar_tab = RightSidebarTab::Effects;
        self.mark_durable_edit();
        true
    }

    #[cfg(test)]
    fn add_cross_dissolve(&mut self, edge: FadeEdge) -> bool {
        self.add_video_transition(edge, VideoTransitionKind::CrossDissolve)
    }

    pub fn delete_selected_timeline_clip(&mut self) -> bool {
        if let Some(title_id) = self.selected_title {
            let before = self.timeline_history_checkpoint();
            if self.timeline.remove_title(title_id).is_ok() {
                self.record_timeline_history(before);
                self.selected_title = None;
                self.title_textures.remove(&title_id);
                self.title_text_drafts.remove(&title_id);
                self.mark_durable_edit();
                return true;
            }
        }
        let Some(clip_id) = self.selected_timeline_clip else {
            return false;
        };
        let before = self.timeline_history_checkpoint();
        let Ok(removed) = self.timeline.delete_clip(clip_id, self.linked_selection) else {
            return false;
        };
        if removed.is_empty() {
            return false;
        }
        self.abandon_provisional_timing(removed.iter().map(|clip| clip.id));
        self.record_timeline_history(before);
        self.selected_timeline_clip = None;
        self.mark_durable_edit();
        true
    }

    /// Toggles the right-clicked/selected clip and its exact linked counterpart when Linked
    /// Selection is active. Disabled clips keep their timeline position and settings but are
    /// omitted from monitor playback, audio playback, and export.
    pub fn set_timeline_clip_enabled(&mut self, clip_id: ClipId, enabled: bool) -> bool {
        let before = self.timeline_history_checkpoint();
        let Ok(changed) = self
            .timeline
            .set_clip_enabled(clip_id, enabled, self.linked_selection)
        else {
            return false;
        };
        if changed.is_empty() {
            return false;
        }
        self.record_timeline_history(before);
        self.mark_durable_edit();
        true
    }

    pub fn razor_at_playhead(&mut self) -> bool {
        let track_id = self
            .selected_timeline_clip
            .and_then(|clip_id| self.timeline.clip(clip_id))
            .map(|clip| clip.track_id)
            .or_else(|| {
                self.timeline
                    .tracks
                    .iter()
                    .find(|track| track.kind == TrackKind::Video)
                    .map(|track| track.id)
            });
        let Some(track_id) = track_id else {
            return false;
        };
        let before = self.timeline_history_checkpoint();
        let splits = if self.linked_selection {
            self.timeline.razor_linked(track_id, self.playhead)
        } else {
            self.timeline
                .razor(track_id, self.playhead)
                .map(|split| split.into_iter().collect())
        };
        let Ok(splits) = splits else { return false };
        let Some(split) = splits.first() else {
            return false;
        };
        self.abandon_provisional_timing(splits.iter().flat_map(|split| [split.left, split.right]));
        self.record_timeline_history(before);
        self.selected_timeline_clip = Some(split.right);
        self.selected_title = None;
        self.mark_durable_edit();
        true
    }

    fn execute_command(&mut self, command: EditorCommand) {
        match command {
            EditorCommand::Undo => {
                self.undo_timeline();
            }
            EditorCommand::Redo => {
                self.redo_timeline();
            }
            EditorCommand::AddVideoTrack => self.add_timeline_track(TrackKind::Video),
            EditorCommand::AddAudioTrack => self.add_timeline_track(TrackKind::Audio),
            EditorCommand::RazorAtPlayhead => {
                self.razor_at_playhead();
            }
            EditorCommand::DeleteSelected => {
                self.delete_selected_timeline_clip();
            }
            EditorCommand::PointerTool => self.tool = TimelineTool::Pointer,
            EditorCommand::RangeTool => self.tool = TimelineTool::Range,
        }
    }

    fn edit_selected_at_playhead(&mut self, mode: EditorEditMode) -> bool {
        let Some(media_id) = self.selected_media else {
            return false;
        };
        let Some(item) = self.media.iter().find(|item| item.id == media_id).cloned() else {
            return false;
        };
        let target = match item.kind {
            MediaKind::Video => EditTarget::VideoAndAudio,
            MediaKind::Audio => EditTarget::AudioOnly,
            MediaKind::Image => EditTarget::VideoOnly,
            MediaKind::Unknown => return false,
        };
        let (duration, provisional) = self.media_placement_duration(media_id);
        let before = self.timeline_history_checkpoint();
        let retained_auto_fit = self.auto_fit_provisional_view;
        let result = match mode {
            EditorEditMode::Insert if self.position_lock => return false,
            EditorEditMode::Insert => self.timeline.insert_edit(
                target,
                TimelineMediaId(media_id),
                self.playhead,
                duration,
                Tick(0),
            ),
            EditorEditMode::Overwrite => self.timeline.overwrite_edit(
                target,
                TimelineMediaId(media_id),
                self.playhead,
                duration,
                Tick(0),
            ),
            EditorEditMode::Replace => {
                let Some(clip_id) = self.selected_timeline_clip else {
                    return false;
                };
                if item.kind == MediaKind::Image {
                    let is_video = self
                        .timeline
                        .clip(clip_id)
                        .and_then(|clip| self.timeline.track(clip.track_id))
                        .is_some_and(|track| track.kind == TrackKind::Video);
                    if !is_video {
                        return false;
                    }
                    self.timeline
                        .replace_clip_media(clip_id, TimelineMediaId(media_id), Tick(0), false)
                        .map(|()| vec![clip_id])
                } else {
                    let affected = self.replace_affected_clips(clip_id);
                    if affected.is_empty()
                        || (item.kind == MediaKind::Audio
                            && affected.iter().any(|clip| {
                                self.timeline
                                    .track(clip.track_id)
                                    .is_some_and(|track| track.kind == TrackKind::Video)
                            }))
                    {
                        return false;
                    }
                    self.timeline
                        .replace_clip_media(
                            clip_id,
                            TimelineMediaId(media_id),
                            Tick(0),
                            self.linked_selection,
                        )
                        .map(|()| vec![clip_id])
                }
            }
        };
        let Ok(inserted) = result else { return false };
        self.abandon_changed_provisional_since(&before.timeline);
        if provisional && !matches!(mode, EditorEditMode::Replace) {
            self.provisional_clip_ids.extend(inserted.iter().copied());
            self.auto_fit_provisional_view |= retained_auto_fit;
        }
        self.record_timeline_history(before);
        self.selected_timeline_clip = inserted.first().copied().or(self.selected_timeline_clip);
        self.mark_durable_edit();
        self.emit(EditorAction::AnalyzeMedia {
            media_id,
            path: item.path,
        });
        true
    }

    pub fn visible_time_seconds(&self) -> f32 {
        self.timeline_view_span.0.max(1) as f32 / 1_000_000.0
    }

    fn set_full_extent_zoom(&mut self) {
        let abandoned_auto_fit = std::mem::take(&mut self.auto_fit_provisional_view);
        if self.apply_full_extent_view() || abandoned_auto_fit {
            self.mark_durable_edit();
        }
    }

    /// Recomputes the visible project extent without owning persistence bookkeeping. Placement
    /// can combine the resulting view change with its one durable generation bump.
    fn apply_full_extent_view(&mut self) -> bool {
        let previous = (self.timeline_view_start, self.timeline_view_span);
        (self.timeline_view_start, self.timeline_view_span) = self.full_extent_view();
        previous != (self.timeline_view_start, self.timeline_view_span)
    }

    fn timeline_view_matches_full_extent(&self) -> bool {
        (self.timeline_view_start, self.timeline_view_span) == self.full_extent_view()
    }

    fn full_extent_view(&self) -> (Tick, Tick) {
        let (minimum, maximum) = self
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .fold((i64::MAX, 0_i64), |(minimum, maximum), clip| {
                (minimum.min(clip.start.0), maximum.max(clip.end().0))
            });
        if minimum == i64::MAX {
            return (Tick(0), legacy_zoom_span(self.zoom_left, self.zoom_right));
        }
        let content_span = (maximum - minimum).max(1_000_000);
        let margin = (content_span / 20).max(1_000_000);
        let start = Tick(minimum.saturating_sub(margin).max(0));
        let span = Tick(
            maximum
                .saturating_add(margin)
                .saturating_sub(start.0)
                .max(1),
        );
        (start, span)
    }

    fn set_detail_zoom(&mut self) {
        self.center_timeline_view(Tick(15_000_000));
    }

    fn set_custom_timeline_view(&mut self) {
        self.center_timeline_view(legacy_zoom_span(self.zoom_left, self.zoom_right));
    }

    fn center_timeline_view(&mut self, span: Tick) {
        let previous = (self.timeline_view_start, self.timeline_view_span);
        let abandoned_auto_fit = std::mem::take(&mut self.auto_fit_provisional_view);
        self.timeline_view_span = Tick(span.0.max(1));
        self.timeline_view_start = Tick(
            self.playhead
                .0
                .saturating_sub(self.timeline_view_span.0 / 2)
                .max(0),
        );
        if previous != (self.timeline_view_start, self.timeline_view_span) || abandoned_auto_fit {
            self.mark_durable_edit();
        }
    }

    fn pan_timeline_view(&mut self, delta: Tick) {
        let abandoned_auto_fit = std::mem::take(&mut self.auto_fit_provisional_view);
        let next = Tick(self.timeline_view_start.0.saturating_add(delta.0).max(0));
        if next != self.timeline_view_start || abandoned_auto_fit {
            self.timeline_view_start = next;
            self.mark_durable_edit();
        }
    }

    fn zoom_timeline_view(&mut self, anchor: Tick, anchor_fraction: f32, factor: f32) {
        const MIN_SPAN: i64 = 250_000;
        const MAX_SPAN: i64 = i64::MAX / 8;
        let previous = (self.timeline_view_start, self.timeline_view_span);
        let abandoned_auto_fit = std::mem::take(&mut self.auto_fit_provisional_view);
        let new_span = ((self.timeline_view_span.0.max(1) as f64 * factor as f64).round() as i64)
            .clamp(MIN_SPAN, MAX_SPAN);
        let fraction = anchor_fraction.clamp(0.0, 1.0) as f64;
        let start = (anchor.0 as f64 - new_span as f64 * fraction)
            .round()
            .clamp(0.0, (i64::MAX - new_span) as f64) as i64;
        self.timeline_view_start = Tick(start);
        self.timeline_view_span = Tick(new_span);
        if previous != (self.timeline_view_start, self.timeline_view_span) || abandoned_auto_fit {
            self.mark_durable_edit();
        }
    }

    fn replace_affected_clips(&self, clip_id: ClipId) -> Vec<&nle_timeline::Clip> {
        let Some(selected) = self.timeline.clip(clip_id) else {
            return Vec::new();
        };
        if !self.linked_selection {
            return vec![selected];
        }
        selected.link_id.map_or_else(
            || vec![selected],
            |link_id| {
                self.timeline
                    .tracks
                    .iter()
                    .flat_map(|track| track.clips.iter())
                    .filter(|clip| {
                        clip.link_id == Some(link_id)
                            && clip.start == selected.start
                            && clip.duration == selected.duration
                    })
                    .collect()
            },
        )
    }

    fn emit(&mut self, action: EditorAction) {
        self.action = Some(action);
    }

    fn selected(&self) -> Option<&MediaItem> {
        self.selected_media
            .and_then(|id| self.media.iter().find(|item| item.id == id))
    }

    fn refresh_media_filter(&mut self) {
        self.filter_query = self.search.to_lowercase();
        self.filtered_media.clear();
        self.filtered_media.extend(
            self.media
                .iter()
                .enumerate()
                .filter(|(_, item)| item.search_name.contains(&self.filter_query))
                .map(|(index, _)| index),
        );
    }
}

fn media_item(id: MediaId, path: PathBuf, duration: Option<Tick>) -> MediaItem {
    let kind = classify_path(&path);
    let display_name = display_name(&path).to_owned();
    MediaItem {
        id,
        kind,
        path,
        duration: duration.filter(|duration| duration.0 > 0),
        search_name: display_name.to_lowercase(),
        label: format!("{}  {display_name}", kind_icon(kind)),
        display_name,
    }
}

fn finite_or(value: f32, default: f32) -> f32 {
    if value.is_finite() { value } else { default }
}

fn duration_seconds_to_tick(seconds: f64) -> Option<Tick> {
    (seconds.is_finite() && seconds > 0.0)
        .then(|| Tick((seconds * 1_000_000.0).round().clamp(1.0, i64::MAX as f64) as i64))
}

pub fn classify_path(path: &Path) -> MediaKind {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "mxf" | "m4v" => MediaKind::Video,
        "wav" | "mp3" | "aiff" | "aif" | "flac" | "aac" | "m4a" | "ogg" => MediaKind::Audio,
        "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff" | "bmp" | "gif" | "exr" => {
            MediaKind::Image
        }
        _ => MediaKind::Unknown,
    }
}

/// HOT PATH — no IO. Rebuilds editor chrome and a deterministic fallback timeline canvas.
pub fn show_editor(ui: &mut Ui, state: &mut EditorState) {
    let mut timeline_canvas = EguiTimelineCanvas::default();
    let mut viewer_canvas = EguiViewerCanvas::default();
    show_editor_with_canvases(ui, state, &mut timeline_canvas, &mut viewer_canvas);
}

// HOT PATH — no IO. Pure arithmetic for the current viewport.
fn responsive_default_timeline_height(remaining_height: f32) -> f32 {
    let max_timeline_height = (remaining_height - 190.0).max(180.0);
    let min_timeline_height = MIN_COMPLETE_TIMELINE_PANEL_HEIGHT.min(max_timeline_height);
    (remaining_height * DEFAULT_TIMELINE_HEIGHT_FRACTION)
        .clamp(min_timeline_height, max_timeline_height)
}

/// Renders the editor with an application-provided solid timeline canvas.
///
/// Callers with a native renderer should install their egui paint callback from `begin`; the
/// callback is deliberately registered before UI-core emits texture, text and curve overlays.
/// HOT PATH — no IO. Emits only immediate UI and retained native timeline primitives.
pub fn show_editor_with_timeline_canvas(
    ui: &mut Ui,
    state: &mut EditorState,
    canvas: &mut dyn TimelineCanvas,
) {
    let mut viewer_canvas = EguiViewerCanvas::default();
    show_editor_with_canvases(ui, state, canvas, &mut viewer_canvas);
}

/// Renders the editor through application-provided retained timeline and viewer canvases.
/// HOT PATH — no IO. Geometry and fallback texture handles are the only cross-layer data.
pub fn show_editor_with_canvases(
    ui: &mut Ui,
    state: &mut EditorState,
    timeline_canvas: &mut dyn TimelineCanvas,
    viewer_canvas: &mut dyn ViewerCanvas,
) {
    editor_style(ui.ctx());
    claim_media_press_from_previous_layout(ui, state);
    handle_editor_shortcuts(ui, state);
    handle_timeline_tool_shortcuts(ui, state);
    if state.playing {
        state.advance_playback(Duration::from_secs_f32(
            ui.input(|input| input.stable_dt).min(0.25),
        ));
        if state.playing {
            ui.ctx().request_repaint();
        }
    }
    Frame::new()
        .fill(Color32::from_rgb(11, 14, 19))
        .inner_margin(egui::Margin::same(EDITOR_OUTER_INSET))
        .show(ui, |ui| {
            // The frame already reserves the outer inset. Reusing the pre-frame size here would
            // force content back through that inset and clip right/bottom labels.
            ui.set_min_size(ui.available_size());
            top_bar(ui, state);
            ui.add_space(4.0);
            const WORKSPACE_NAV_HEIGHT: f32 = 38.0;
            let available_body = Vec2::new(
                ui.available_width(),
                (ui.available_height() - WORKSPACE_NAV_HEIGHT).max(220.0),
            );
            ui.allocate_ui_with_layout(available_body, Layout::top_down(Align::LEFT), |ui| {
                match state.workspace {
                    EditorWorkspace::Edit => {
                        let max_timeline_height = (available_body.y - 190.0).max(180.0);
                        let timeline_height = if state.timeline_height_is_default {
                            responsive_default_timeline_height(available_body.y)
                        } else {
                            state.timeline_height.clamp(180.0, max_timeline_height)
                        };
                        let viewer_workspace_height =
                            (available_body.y - timeline_height - SPLITTER_THICKNESS).max(180.0);
                        state.rendered_panel_heights =
                            Some((viewer_workspace_height, timeline_height));
                        ui.allocate_ui_with_layout(
                            Vec2::new(available_body.x, viewer_workspace_height),
                            Layout::top_down(Align::LEFT),
                            |ui| main_workspace(ui, state, viewer_canvas),
                        );
                        let splitter = horizontal_splitter(ui, available_body.x);
                        if splitter.dragged() {
                            let delta = ui.input(|input| input.pointer.delta().y);
                            let height =
                                (timeline_height - delta).clamp(180.0, max_timeline_height);
                            if state.timeline_height != height || state.timeline_height_is_default {
                                state.timeline_height = height;
                                state.timeline_height_is_default = false;
                                state.mark_durable_edit();
                            }
                        }
                        timeline_with_canvas(ui, state, timeline_height - 8.0, timeline_canvas);
                    }
                    EditorWorkspace::Undertow => {
                        undertow_workspace(ui, state, available_body, timeline_canvas);
                    }
                    EditorWorkspace::KrakenUpscale => {
                        kraken_upscale_workspace(ui, state);
                    }
                }
            });
            workspace_navigation(ui, state);
        });
    if state.show_licenses {
        egui::Window::new(t(
            state.language,
            "Third-party licenses",
            "サードパーティライセンス",
        ))
        .open(&mut state.show_licenses)
        .default_width(560.0)
        .default_height(420.0)
        .resizable(true)
        .show(ui.ctx(), |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.label(RichText::new(THIRD_PARTY_NOTICES).monospace().small());
            });
        });
    }
    show_command_palette(ui, state);
}

fn workspace_navigation(ui: &mut Ui, state: &mut EditorState) {
    Frame::new()
        .fill(Color32::from_rgb(14, 18, 24))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add_space((ui.available_width() - 560.0).max(0.0) * 0.5);
                if ui
                    .selectable_label(
                        state.workspace == EditorWorkspace::Edit,
                        t(state.language, "▣  Edit", "▣  編集"),
                    )
                    .on_hover_text(t(
                        state.language,
                        "Open the video editing workspace",
                        "ビデオ編集ワークスペースを開く",
                    ))
                    .clicked()
                {
                    state.set_workspace(EditorWorkspace::Edit);
                }
                ui.add_space(16.0);
                if ui
                    .selectable_label(
                        state.workspace == EditorWorkspace::Undertow,
                        t(state.language, "♪  Undertow", "♪  アンダートウ"),
                    )
                    .on_hover_text(t(
                        state.language,
                        "Open the dedicated audio editing workspace",
                        "専用オーディオ編集ワークスペースを開く",
                    ))
                    .clicked()
                {
                    state.set_workspace(EditorWorkspace::Undertow);
                }
                ui.add_space(16.0);
                let upscale_response = ui
                    .add_enabled_ui(state.kraken_upscale_ready, |ui| {
                        ui.selectable_label(
                            state.workspace == EditorWorkspace::KrakenUpscale,
                            t(state.language, "🦑  Kraken Upscale", "🦑  Kraken Upscale"),
                        )
                    })
                    .inner
                    .on_hover_text(if state.kraken_upscale_ready {
                        t(
                            state.language,
                            "NVIDIA RTX VSR upscale workspace",
                            "NVIDIA RTX VSR アップスケール",
                        )
                    } else if state.kraken_upscale_reason.is_empty() {
                        t(
                            state.language,
                            "Requires an NVIDIA RTX GPU and the RTX VSR runtime",
                            "NVIDIA RTX GPU と RTX VSR ランタイムが必要です",
                        )
                    } else {
                        state.kraken_upscale_reason.clone()
                    });
                if upscale_response.clicked() {
                    state.set_workspace(EditorWorkspace::KrakenUpscale);
                }
            });
        });
}

fn kraken_upscale_workspace(ui: &mut Ui, state: &mut EditorState) {
    let subtitle = if state.kraken_upscale_ready {
        t(state.language, "NVIDIA RTX VSR", "NVIDIA RTX VSR")
    } else {
        t(state.language, "Unavailable", "利用不可")
    };
    panel_title(
        ui,
        &t(state.language, "Kraken Upscale", "Kraken Upscale"),
        &subtitle,
    );
    if !state.kraken_upscale_ready {
        ui.label(
            RichText::new(if state.kraken_upscale_reason.is_empty() {
                t(
                    state.language,
                    "Requires an NVIDIA RTX GPU and the RTX VSR runtime (rtx-vsr folder).",
                    "NVIDIA RTX GPU と RTX VSR ランタイム（rtx-vsr フォルダ）が必要です。",
                )
            } else {
                state.kraken_upscale_reason.clone()
            })
            .color(Color32::from_rgb(160, 176, 190)),
        );
        return;
    }
    ui.label(
        RichText::new(&state.kraken_upscale_reason)
            .small()
            .color(Color32::from_rgb(117, 201, 237)),
    );
    ui.add_space(8.0);
    let source = state
        .selected()
        .map(|item| item.path.display().to_string())
        .or_else(|| {
            state
                .selected_timeline_clip
                .and_then(|clip_id| state.timeline.clip(clip_id))
                .and_then(|clip| {
                    state
                        .media
                        .iter()
                        .find(|item| item.id == clip.media.0)
                        .map(|item| item.path.display().to_string())
                })
        })
        .unwrap_or_else(|| {
            t(
                state.language,
                "Select a video clip or media card",
                "ビデオクリップまたはメディアを選択",
            )
        });
    ui.label(
        RichText::new(t(state.language, "Source", "ソース"))
            .small()
            .strong(),
    );
    ui.label(RichText::new(source).small().monospace());
    ui.add_space(10.0);
    ui.label(
        RichText::new(t(state.language, "Output goal", "出力目標"))
            .small()
            .strong(),
    );
    ui.horizontal(|ui| {
        for (index, (en, jp)) in [
            ("1080p", "1080p"),
            ("1440p", "1440p"),
            ("4K / 2160p", "4K / 2160p"),
            ("×2", "×2"),
            ("×3", "×3"),
            ("×4", "×4"),
        ]
        .into_iter()
        .enumerate()
        {
            if ui
                .selectable_label(
                    state.kraken_upscale_goal == index as u8,
                    t(state.language, en, jp),
                )
                .clicked()
            {
                state.kraken_upscale_goal = index as u8;
            }
        }
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(t(state.language, "Quality", "品質"))
            .small()
            .strong(),
    );
    ui.horizontal(|ui| {
        for (index, (en, jp)) in [
            ("Low", "低"),
            ("Medium", "中"),
            ("High", "高"),
            ("Ultra", "Ultra"),
        ]
        .into_iter()
        .enumerate()
        {
            if ui
                .selectable_label(
                    state.kraken_upscale_quality == index as u8,
                    t(state.language, en, jp),
                )
                .clicked()
            {
                state.kraken_upscale_quality = index as u8;
            }
        }
    });
    ui.add_space(12.0);
    match &state.kraken_upscale_status {
        EditorExportStatus::Running { progress } => {
            ui.label(format!(
                "{}  {:>3.0}%",
                t(state.language, "Upscaling…", "アップスケール中…"),
                progress * 100.0
            ));
            if ui
                .button(t(state.language, "Cancel", "キャンセル"))
                .clicked()
            {
                state.emit(EditorAction::CancelKrakenUpscale);
            }
        }
        EditorExportStatus::Completed(path) => {
            ui.label(
                RichText::new(t(state.language, "Upscale complete", "完了"))
                    .color(Color32::from_rgb(83, 191, 126)),
            );
            ui.label(
                RichText::new(path.display().to_string())
                    .small()
                    .monospace(),
            );
        }
        EditorExportStatus::Failed(error) => {
            ui.label(
                RichText::new(t(state.language, "Upscale failed", "失敗"))
                    .color(Color32::from_rgb(232, 116, 116)),
            )
            .on_hover_text(error);
        }
        EditorExportStatus::Idle => {}
    }
    if !matches!(
        state.kraken_upscale_status,
        EditorExportStatus::Running { .. }
    ) && ui
        .add_enabled(
            state.selected().is_some() || state.selected_timeline_clip.is_some(),
            egui::Button::new(t(state.language, "Upscale", "アップスケール")),
        )
        .clicked()
    {
        state.emit(EditorAction::StartKrakenUpscale);
    }
}

fn handle_editor_shortcuts(ui: &Ui, state: &mut EditorState) {
    let (c_down, open_palette, undo, redo, delete, razor) = ui.input(|input| {
        (
            input.key_down(egui::Key::C),
            input.modifiers.command && input.key_pressed(egui::Key::P),
            input.modifiers.command && !input.modifiers.shift && input.key_pressed(egui::Key::Z),
            (input.modifiers.command && input.key_pressed(egui::Key::Y))
                || (input.modifiers.command
                    && input.modifiers.shift
                    && input.key_pressed(egui::Key::Z)),
            input.key_pressed(egui::Key::Delete),
            input.modifiers.command && input.key_pressed(egui::Key::B),
        )
    });
    if state.held_razor && !c_down {
        state.held_razor = false;
        state.tool = TimelineTool::Pointer;
    }
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    if c_down && !state.held_razor {
        state.held_razor = true;
        state.tool = TimelineTool::Razor;
    }
    if open_palette {
        state.open_command_palette();
    } else if undo {
        state.undo_timeline();
    } else if redo {
        state.redo_timeline();
    } else if delete {
        state.delete_selected_timeline_clip();
    } else if razor {
        state.razor_at_playhead();
    }
}

fn handle_timeline_tool_shortcuts(ui: &Ui, state: &mut EditorState) {
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let input = ui.input(|input| {
        (
            input.key_pressed(egui::Key::A),
            input.key_pressed(egui::Key::R),
            input.key_pressed(egui::Key::T),
            input.key_pressed(egui::Key::B),
            input.key_pressed(egui::Key::W),
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::F9),
            input.key_pressed(egui::Key::F10),
            input.key_pressed(egui::Key::F11),
            input.key_pressed(egui::Key::J) || input.key_pressed(egui::Key::Comma),
            input.key_pressed(egui::Key::L) || input.key_pressed(egui::Key::Period),
        )
    });
    if input.0 {
        state.tool = TimelineTool::Pointer;
    }
    if input.1 {
        state.tool = TimelineTool::Range;
    }
    if input.2 {
        state.tool = TimelineTool::Trim;
    }
    if input.3 {
        state.tool = TimelineTool::Razor;
    }
    if input.4 {
        state.tool = TimelineTool::DynamicTrim;
    }
    if input.5 {
        state.tool = TimelineTool::Slip;
    }
    if input.6 {
        state.snapping = !state.snapping;
        state.mark_durable_edit();
    }
    if input.7 {
        let _ = state.edit_selected_at_playhead(EditorEditMode::Insert);
    }
    if input.8 {
        let _ = state.edit_selected_at_playhead(EditorEditMode::Overwrite);
    }
    if input.9 {
        let _ = state.edit_selected_at_playhead(EditorEditMode::Replace);
    }
    if state.tool == TimelineTool::DynamicTrim
        && let Some(clip_id) = state.selected_timeline_clip
    {
        if input.10 && dynamic_trim_selected(state, clip_id, -1) {
            state.mark_durable_edit();
        }
        if input.11 && dynamic_trim_selected(state, clip_id, 1) {
            state.mark_durable_edit();
        }
    }
}

fn dynamic_trim_selected(state: &mut EditorState, clip_id: ClipId, direction: i64) -> bool {
    let before = state.timeline_history_checkpoint();
    let affected = state
        .replace_affected_clips(clip_id)
        .into_iter()
        .map(|clip| clip.id)
        .collect::<Vec<_>>();
    let changed = state
        .dynamic_trim_delta(clip_id, direction)
        .is_some_and(|delta| {
            state
                .timeline
                .trim_end(clip_id, delta, state.linked_selection, !state.position_lock)
                .is_ok()
        });
    if changed {
        state.abandon_provisional_timing(affected);
        state.record_timeline_history(before);
    }
    changed
}

fn show_command_palette(ui: &Ui, state: &mut EditorState) {
    if !state.command_palette_open {
        return;
    }
    let mut open = true;
    let mut close_requested = false;
    let mut execute = None;
    let mut visible = EDITOR_COMMANDS
        .iter()
        .copied()
        .filter(|command| command_matches(*command, &state.command_query, state.language))
        .collect::<Vec<_>>();
    egui::Window::new(t(state.language, "Command palette", "コマンドパレット"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(440.0)
        .anchor(egui::Align2::CENTER_TOP, Vec2::new(0.0, 72.0))
        .show(ui.ctx(), |ui| {
            let search = ui.add(
                egui::TextEdit::singleline(&mut state.command_query)
                    .hint_text(t(state.language, "Type a command…", "コマンドを入力…"))
                    .desired_width(f32::INFINITY),
            );
            search.request_focus();
            visible = EDITOR_COMMANDS
                .iter()
                .copied()
                .filter(|command| command_matches(*command, &state.command_query, state.language))
                .collect();
            if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                close_requested = true;
            }
            if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                execute = visible.first().map(|command| command.command);
            }
            ui.separator();
            for command in &visible {
                let name = t(state.language, command.english, command.japanese);
                let label = command.key.map_or_else(
                    || format!("{name}  ·  {}", command.id),
                    |key| format!("{name}                                      {key}"),
                );
                if ui.selectable_label(false, label).clicked() {
                    execute = Some(command.command);
                }
            }
            if visible.is_empty() {
                ui.label(t(
                    state.language,
                    "No matching commands",
                    "一致するコマンドなし",
                ));
            }
        });
    if close_requested {
        open = false;
    }
    if let Some(command) = execute {
        state.execute_command(command);
        open = false;
    }
    state.command_palette_open = open;
}

fn command_matches(command: CommandSpec, query: &str, language: Language) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {}",
        command.id,
        command.english,
        t(language, command.english, command.japanese)
    )
    .to_lowercase();
    let mut characters = haystack.chars();
    query
        .chars()
        .all(|needle| characters.by_ref().any(|candidate| candidate == needle))
}

const SPLITTER_THICKNESS: f32 = 7.0;

fn editor_style(ctx: &egui::Context) {
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals.panel_fill = Color32::from_rgb(11, 14, 19);
        style.visuals.window_fill = Color32::from_rgb(19, 24, 31);
        style.visuals.extreme_bg_color = Color32::from_rgb(7, 9, 13);
        style.visuals.faint_bg_color = Color32::from_rgb(20, 26, 34);
        style.visuals.selection.bg_fill = Color32::from_rgb(23, 76, 108);
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(24, 31, 40);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(30, 48, 61);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(24, 81, 113);
        style.visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(202, 213, 225);
        style.spacing.item_spacing = Vec2::new(7.0, 6.0);
    });
}

fn top_bar(ui: &mut Ui, state: &mut EditorState) {
    Frame::new()
        .fill(Color32::from_rgb(17, 22, 29))
        .inner_margin(egui::Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("MAELSTROM")
                        .strong()
                        .color(Color32::from_rgb(117, 201, 237)),
                );
                ui.separator();
                file_menu(ui, state);
                edit_menu(ui, state);
                view_menu(ui, state);
                playback_menu(ui, state);
                help_menu(ui, state);
                ui.separator();
                ui.label(RichText::new(&state.project_name).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let quick_export_block = state.quick_export_block_message();
                    if ui
                        .button(menu_text(state.language, "Projects", "プロジェクト"))
                        .on_hover_text(menu_text(
                            state.language,
                            "Return to the project hub",
                            "プロジェクト一覧に戻る",
                        ))
                        .clicked()
                    {
                        state.emit(EditorAction::ReturnToHub);
                    }
                    match &state.export_status {
                        EditorExportStatus::Running { progress } => {
                            let progress = *progress;
                            if ui
                                .button(menu_text(state.language, "Cancel Export", "書き出し中止"))
                                .on_hover_text(menu_text(
                                    state.language,
                                    "Cancel the background export",
                                    "バックグラウンド書き出しを中止",
                                ))
                                .clicked()
                            {
                                state.emit(EditorAction::CancelExport);
                            }
                            ui.add(
                                egui::ProgressBar::new(progress)
                                    .desired_width(110.0)
                                    .text(format!("{:.0}%", progress * 100.0)),
                            );
                        }
                        EditorExportStatus::Completed(path) => {
                            ui.label(
                                RichText::new(menu_text(
                                    state.language,
                                    "Export complete",
                                    "書き出し完了",
                                ))
                                .small()
                                .color(Color32::from_rgb(113, 205, 151)),
                            )
                            .on_hover_text(path.display().to_string());
                            if ui
                                .add_enabled(
                                    quick_export_block.is_none(),
                                    egui::Button::new(menu_text(
                                        state.language,
                                        "Quick Export",
                                        "クイック書き出し",
                                    )),
                                )
                                .on_hover_text(quick_export_block.unwrap_or(match state.language {
                                    Language::English => {
                                        "Export the current composited timeline snapshot"
                                    }
                                    Language::Japanese => {
                                        "現在の合成済みタイムラインスナップショットを書き出す"
                                    }
                                }))
                                .clicked()
                            {
                                state.emit(EditorAction::StartExport);
                            }
                        }
                        EditorExportStatus::Failed(error) => {
                            ui.label(
                                RichText::new(menu_text(
                                    state.language,
                                    "Export failed",
                                    "書き出し失敗",
                                ))
                                .small()
                                .color(Color32::from_rgb(232, 116, 116)),
                            )
                            .on_hover_text(error);
                            if ui
                                .add_enabled(
                                    quick_export_block.is_none(),
                                    egui::Button::new(menu_text(
                                        state.language,
                                        "Retry Export",
                                        "書き出し再試行",
                                    )),
                                )
                                .on_hover_text(quick_export_block.unwrap_or(match state.language {
                                    Language::English => {
                                        "Retry the current composited timeline snapshot"
                                    }
                                    Language::Japanese => {
                                        "現在の合成済みタイムラインスナップショットを再試行"
                                    }
                                }))
                                .clicked()
                            {
                                state.emit(EditorAction::StartExport);
                            }
                        }
                        EditorExportStatus::Idle => {
                            if ui
                                .add_enabled(
                                    quick_export_block.is_none(),
                                    egui::Button::new(menu_text(
                                        state.language,
                                        "Quick Export",
                                        "クイック書き出し",
                                    )),
                                )
                                .on_hover_text(quick_export_block.unwrap_or(match state.language {
                                    Language::English => {
                                        "Export the composited snapshot while continuing to edit"
                                    }
                                    Language::Japanese => {
                                        "編集を続けながら合成済みスナップショットを書き出す"
                                    }
                                }))
                                .clicked()
                            {
                                state.emit(EditorAction::StartExport);
                            }
                        }
                    }
                    ui.label(
                        RichText::new(match state.workspace {
                            EditorWorkspace::Edit => menu_text(state.language, "EDIT", "編集"),
                            EditorWorkspace::Undertow => menu_text(
                                state.language,
                                "UNDERTOW AUDIO",
                                "アンダートウ・オーディオ",
                            ),
                            EditorWorkspace::KrakenUpscale => {
                                menu_text(state.language, "KRAKEN UPSCALE", "KRAKEN UPSCALE")
                            }
                        })
                        .color(Color32::from_rgb(117, 201, 237)),
                    );
                    #[cfg(debug_assertions)]
                    if ui
                        .selectable_label(
                            state.vsync_enabled,
                            if state.vsync_enabled {
                                "VSync ON"
                            } else {
                                "VSync OFF"
                            },
                        )
                        .on_hover_text("Debug: toggle presentation synchronization")
                        .clicked()
                    {
                        state.vsync_enabled = !state.vsync_enabled;
                        state.emit(EditorAction::SetVsync(state.vsync_enabled));
                    }
                    ui.label(
                        RichText::new(&state.performance_hud)
                            .monospace()
                            .small()
                            .color(Color32::from_rgb(126, 148, 164)),
                    )
                    .on_hover_ui(|ui| runtime_diagnostics_hover_ui(ui, state));
                });
            });
        });
}

fn runtime_diagnostics_hover_ui(ui: &mut Ui, state: &EditorState) {
    let diagnostics = state.runtime_diagnostics;
    ui.set_min_width(290.0);
    ui.strong(menu_text(
        state.language,
        "Session diagnostics",
        "セッション診断",
    ));
    ui.label(
        RichText::new(menu_text(
            state.language,
            "Cumulative counters; project media and settings are unchanged.",
            "累積カウンターです。プロジェクトのメディアや設定は変更されません。",
        ))
        .small()
        .color(Color32::from_rgb(146, 163, 176)),
    );
    ui.add_space(4.0);
    egui::Grid::new("runtime-diagnostics-grid")
        .num_columns(2)
        .spacing([18.0, 3.0])
        .striped(true)
        .show(ui, |ui| {
            for (label, value) in [
                (
                    menu_text(state.language, "Decode requests", "デコード要求"),
                    diagnostics.monitor_requests.to_string(),
                ),
                (
                    menu_text(state.language, "Completed frames", "完了フレーム"),
                    diagnostics.monitor_completed_frames.to_string(),
                ),
                (
                    menu_text(state.language, "Presented frames", "表示フレーム"),
                    diagnostics.monitor_presented_frames.to_string(),
                ),
                (
                    menu_text(state.language, "Dropped stale frames", "破棄した旧フレーム"),
                    diagnostics.monitor_dropped_frames.to_string(),
                ),
                (
                    menu_text(state.language, "Retained-frame holds", "保持フレーム待機"),
                    diagnostics.monitor_hold_events.to_string(),
                ),
                (
                    menu_text(state.language, "Late completions", "遅延完了"),
                    diagnostics.monitor_late_frames.to_string(),
                ),
                (
                    menu_text(state.language, "Decode errors", "デコードエラー"),
                    diagnostics.monitor_errors.to_string(),
                ),
                (
                    menu_text(
                        state.language,
                        "Decode turnaround p95",
                        "デコード所要時間 p95",
                    ),
                    format!("{:.2} ms", diagnostics.monitor_turnaround_p95_ms),
                ),
                (
                    menu_text(
                        state.language,
                        "Native / fallback uploads",
                        "ネイティブ / 代替アップロード",
                    ),
                    format!(
                        "{} / {}",
                        diagnostics.native_viewer_uploads, diagnostics.fallback_viewer_uploads
                    ),
                ),
                (
                    menu_text(
                        state.language,
                        "Audio underrun frames",
                        "オーディオ不足フレーム",
                    ),
                    diagnostics.audio_underrun_frames.to_string(),
                ),
                (
                    menu_text(
                        state.language,
                        "Audio callback lock misses",
                        "音声コールバックロック失敗",
                    ),
                    diagnostics.audio_callback_lock_failures.to_string(),
                ),
                (
                    menu_text(
                        state.language,
                        "Late audio frames discarded",
                        "破棄した遅延音声フレーム",
                    ),
                    diagnostics.audio_late_discarded_frames.to_string(),
                ),
            ] {
                ui.label(RichText::new(label).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(value).monospace().small());
                });
                ui.end_row();
            }
        });
}

fn file_menu(ui: &mut Ui, state: &mut EditorState) {
    ui.menu_button(menu_text(state.language, "File", "ファイル"), |ui| {
        let quick_export_block = state.quick_export_block_message();
        if ui
            .button(menu_text(
                state.language,
                "Import Media…",
                "メディアを読み込む…",
            ))
            .clicked()
        {
            state.emit(EditorAction::ChooseMediaFiles);
            ui.close();
        }
        match &state.export_status {
            EditorExportStatus::Running { .. } => {
                if ui
                    .button(menu_text(state.language, "Cancel Export", "書き出しを中止"))
                    .clicked()
                {
                    state.emit(EditorAction::CancelExport);
                    ui.close();
                }
            }
            _ => {
                if ui
                    .add_enabled(
                        quick_export_block.is_none(),
                        egui::Button::new(menu_text(
                            state.language,
                            "Quick Export…",
                            "クイック書き出し…",
                        )),
                    )
                    .on_hover_text(quick_export_block.unwrap_or(match state.language {
                        Language::English => "Export the current composited timeline snapshot",
                        Language::Japanese => {
                            "現在の合成済みタイムラインスナップショットを書き出す"
                        }
                    }))
                    .clicked()
                {
                    state.emit(EditorAction::StartExport);
                    ui.close();
                }
            }
        }
        ui.separator();
        ui.label(
            RichText::new(menu_text(
                state.language,
                "Project changes are saved automatically",
                "プロジェクトの変更は自動保存されます",
            ))
            .small()
            .color(Color32::from_rgb(126, 148, 164)),
        );
        if ui
            .button(menu_text(
                state.language,
                "Return to Project Hub",
                "プロジェクト一覧に戻る",
            ))
            .clicked()
        {
            state.emit(EditorAction::ReturnToHub);
            ui.close();
        }
    });
}

fn edit_menu(ui: &mut Ui, state: &mut EditorState) {
    ui.menu_button(menu_text(state.language, "Edit", "編集"), |ui| {
        if ui
            .add_enabled(
                state.history.can_undo(),
                egui::Button::new(menu_text(
                    state.language,
                    "Undo    Ctrl+Z",
                    "元に戻す    Ctrl+Z",
                )),
            )
            .clicked()
        {
            state.undo_timeline();
            ui.close();
        }
        if ui
            .add_enabled(
                state.history.can_redo(),
                egui::Button::new(menu_text(
                    state.language,
                    "Redo    Ctrl+Y",
                    "やり直す    Ctrl+Y",
                )),
            )
            .clicked()
        {
            state.redo_timeline();
            ui.close();
        }
        ui.separator();
        let has_selection = state.selected_timeline_clip.is_some();
        if ui
            .add_enabled(
                has_selection,
                egui::Button::new(menu_text(
                    state.language,
                    "Razor at Playhead    Ctrl+B",
                    "再生ヘッドで分割    Ctrl+B",
                )),
            )
            .clicked()
        {
            state.razor_at_playhead();
            ui.close();
        }
        if ui
            .add_enabled(
                has_selection,
                egui::Button::new(menu_text(
                    state.language,
                    "Delete Selected    Delete",
                    "選択項目を削除    Delete",
                )),
            )
            .clicked()
        {
            state.delete_selected_timeline_clip();
            ui.close();
        }
        ui.separator();
        if ui
            .button(menu_text(
                state.language,
                "Command Palette…    Ctrl+P",
                "コマンドパレット…    Ctrl+P",
            ))
            .clicked()
        {
            state.command_palette_open = true;
            state.command_query.clear();
            ui.close();
        }
    });
}

fn view_menu(ui: &mut Ui, state: &mut EditorState) {
    ui.menu_button(menu_text(state.language, "View", "表示"), |ui| {
        if ui
            .button(menu_text(state.language, "Full Extent", "全体表示"))
            .clicked()
        {
            state.set_full_extent_zoom();
            ui.close();
        }
        if ui
            .button(menu_text(
                state.language,
                "Detail at Playhead",
                "再生ヘッドを詳細表示",
            ))
            .clicked()
        {
            state.set_detail_zoom();
            ui.close();
        }
        if ui
            .button(menu_text(state.language, "Custom Zoom", "カスタムズーム"))
            .clicked()
        {
            state.set_custom_timeline_view();
            ui.close();
        }
        ui.separator();
        if ui
            .checkbox(
                &mut state.show_video_thumbnails,
                menu_text(state.language, "Video Thumbnails", "ビデオサムネイル"),
            )
            .changed()
        {
            state.mark_durable_edit();
        }
        if ui
            .checkbox(
                &mut state.show_audio_waveforms,
                menu_text(state.language, "Audio Waveforms", "オーディオ波形"),
            )
            .changed()
        {
            state.mark_durable_edit();
        }
        ui.menu_button(
            menu_text(state.language, "Track Height", "トラックの高さ"),
            |ui| {
                for (density, english, japanese) in [
                    (TimelineTrackDensity::Compact, "Compact", "コンパクト"),
                    (TimelineTrackDensity::Normal, "Normal", "標準"),
                    (TimelineTrackDensity::Large, "Large", "大"),
                ] {
                    if ui
                        .selectable_label(
                            state.track_density == density,
                            menu_text(state.language, english, japanese),
                        )
                        .clicked()
                    {
                        state.set_track_density(density);
                        ui.close();
                    }
                }
            },
        );
        ui.separator();
        if ui
            .button(menu_text(
                state.language,
                "Reset Workspace Layout",
                "ワークスペース配置をリセット",
            ))
            .clicked()
        {
            state.reset_workspace_layout();
            ui.close();
        }
    });
}

fn playback_menu(ui: &mut Ui, state: &mut EditorState) {
    ui.menu_button(menu_text(state.language, "Playback", "再生"), |ui| {
        ui.menu_button(
            menu_text(state.language, "Playback Resolution", "再生解像度"),
            |ui| {
                preview_quality_menu(ui, state, false);
            },
        );
        ui.menu_button(
            menu_text(state.language, "Paused Resolution", "停止時解像度"),
            |ui| {
                preview_quality_menu(ui, state, true);
            },
        );
        ui.separator();
        let mut high_quality = state.high_quality_playback();
        if ui
            .checkbox(
                &mut high_quality,
                menu_text(state.language, "High Quality Playback", "高品質再生"),
            )
            .changed()
        {
            state.set_high_quality_playback(high_quality);
        }
        ui.separator();
        if ui
            .button(menu_text(
                state.language,
                "Play / Pause    Space",
                "再生 / 一時停止    Space",
            ))
            .clicked()
        {
            state.toggle_playback();
            ui.close();
        }
        if ui
            .button(menu_text(state.language, "Stop", "停止"))
            .clicked()
        {
            state.playing = false;
            ui.close();
        }
        if ui
            .button(menu_text(state.language, "Go to Start", "先頭へ移動"))
            .clicked()
        {
            state.set_playhead_inner(Tick(0), false);
            ui.close();
        }
        if ui
            .button(menu_text(state.language, "Previous Frame", "前のフレーム"))
            .clicked()
        {
            let previous = state
                .frame_rate
                .frame_index_at_tick(Tick(state.playhead.0.saturating_sub(1)));
            state.set_playhead_inner(state.frame_rate.frame_boundary_tick(previous), false);
            ui.close();
        }
        if ui
            .button(menu_text(state.language, "Next Frame", "次のフレーム"))
            .clicked()
        {
            let next = state
                .frame_rate
                .frame_index_at_tick(state.playhead)
                .saturating_add(1);
            state.set_playhead_inner(state.frame_rate.frame_boundary_tick(next), false);
            ui.close();
        }
        if ui
            .button(menu_text(state.language, "Go to End", "終端へ移動"))
            .clicked()
        {
            state.set_playhead_inner(state.timeline_end(), false);
            ui.close();
        }
    });
}

fn preview_quality_menu(ui: &mut Ui, state: &mut EditorState, paused: bool) {
    let selected = if paused {
        state.paused_preview_quality()
    } else {
        state.preview_quality()
    };
    for &quality in preview_quality_menu_choices(paused) {
        if ui
            .selectable_label(
                selected == quality,
                preview_quality_option_label(state.language, quality),
            )
            .clicked()
        {
            if paused {
                state.set_paused_preview_quality(quality);
            } else {
                state.set_preview_quality(quality);
            }
            ui.close();
        }
    }
}

const fn preview_quality_menu_choices(paused: bool) -> &'static [PreviewQuality] {
    if paused {
        &[
            PreviewQuality::Full,
            PreviewQuality::Half,
            PreviewQuality::Quarter,
            PreviewQuality::Eighth,
        ]
    } else {
        &[
            PreviewQuality::Auto,
            PreviewQuality::Full,
            PreviewQuality::Half,
            PreviewQuality::Quarter,
            PreviewQuality::Eighth,
        ]
    }
}

fn help_menu(ui: &mut Ui, state: &mut EditorState) {
    ui.menu_button(menu_text(state.language, "Help", "ヘルプ"), |ui| {
        if ui
            .button(menu_text(
                state.language,
                "Licenses and Notices",
                "ライセンスと通知",
            ))
            .clicked()
        {
            state.show_licenses = true;
            ui.close();
        }
    });
}

fn main_workspace(ui: &mut Ui, state: &mut EditorState, viewer_canvas: &mut dyn ViewerCanvas) {
    let width = ui.available_width();
    let height = ui.available_height();
    let splitter_total = SPLITTER_THICKNESS * 2.0;
    let left_width = state
        .media_pool_width
        .clamp(190.0, (width - splitter_total - 220.0 - 220.0).max(190.0));
    let right_width = state.analysis_width.clamp(
        220.0,
        (width - splitter_total - left_width - 220.0).max(220.0),
    );
    if state.media_pool_width != left_width {
        state.media_pool_width = left_width;
        state.mark_durable_edit();
    }
    if state.analysis_width != right_width {
        state.analysis_width = right_width;
        state.mark_durable_edit();
    }
    let center_width = (width - left_width - right_width - splitter_total).max(0.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.allocate_ui_with_layout(
            Vec2::new(left_width, height),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_clip_rect(ui.max_rect());
                panel_frame(ui);
                media_pool(ui, state);
            },
        );
        let left_splitter = vertical_splitter(ui, height);
        if left_splitter.dragged() {
            let delta = ui.input(|input| input.pointer.delta().x);
            let next_width = (state.media_pool_width + delta).clamp(
                190.0,
                (width - splitter_total - state.analysis_width - 220.0).max(190.0),
            );
            if state.media_pool_width != next_width {
                state.media_pool_width = next_width;
                state.mark_durable_edit();
            }
        }
        ui.allocate_ui_with_layout(
            Vec2::new(center_width, height),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_clip_rect(ui.max_rect());
                panel_frame(ui);
                viewer_with_canvas(ui, state, viewer_canvas);
            },
        );
        let right_splitter = vertical_splitter(ui, height);
        if right_splitter.dragged() {
            let delta = ui.input(|input| input.pointer.delta().x);
            let next_width = (state.analysis_width - delta).clamp(
                220.0,
                (width - splitter_total - state.media_pool_width - 220.0).max(220.0),
            );
            if state.analysis_width != next_width {
                state.analysis_width = next_width;
                state.mark_durable_edit();
            }
        }
        ui.allocate_ui_with_layout(
            Vec2::new(right_width, height),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_clip_rect(ui.max_rect());
                panel_frame(ui);
                details(ui, state);
            },
        );
    });
}

fn undertow_workspace(
    ui: &mut Ui,
    state: &mut EditorState,
    size: Vec2,
    canvas: &mut dyn TimelineCanvas,
) {
    let focused_track = state.ensure_undertow_track();
    let splitter_total = SPLITTER_THICKNESS * 2.0;
    let left_width = state
        .undertow_tools_width
        .clamp(150.0, (size.x - splitter_total - 260.0 - 180.0).max(150.0));
    let right_width = state.undertow_mixer_width.clamp(
        180.0,
        (size.x - splitter_total - left_width - 260.0).max(180.0),
    );
    if state.undertow_tools_width != left_width || state.undertow_mixer_width != right_width {
        state.undertow_tools_width = left_width;
        state.undertow_mixer_width = right_width;
        state.mark_durable_edit();
    }
    let center_width = (size.x - left_width - right_width - SPLITTER_THICKNESS * 2.0).max(260.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.allocate_ui_with_layout(
            Vec2::new(left_width, size.y),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_clip_rect(ui.max_rect());
                panel_frame(ui);
                undertow_tools_and_tracks(ui, state);
            },
        );
        let left_splitter = vertical_splitter(ui, size.y);
        if left_splitter.dragged() {
            let delta = ui.input(|input| input.pointer.delta().x);
            let width = (left_width + delta).clamp(
                150.0,
                (size.x - splitter_total - right_width - 260.0).max(150.0),
            );
            if state.undertow_tools_width != width {
                state.undertow_tools_width = width;
                state.mark_durable_edit();
            }
        }
        ui.allocate_ui_with_layout(
            Vec2::new(center_width, size.y),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_clip_rect(ui.max_rect());
                panel_frame(ui);
                undertow_transport(ui, state);
                timeline_with_canvas_presentation(
                    ui,
                    state,
                    (size.y - 108.0).max(250.0),
                    canvas,
                    TimelinePresentation {
                        show_tool_row: false,
                        audio_focus: focused_track,
                    },
                );
            },
        );
        let right_splitter = vertical_splitter(ui, size.y);
        if right_splitter.dragged() {
            let delta = ui.input(|input| input.pointer.delta().x);
            let width = (right_width - delta).clamp(
                180.0,
                (size.x - splitter_total - left_width - 260.0).max(180.0),
            );
            if state.undertow_mixer_width != width {
                state.undertow_mixer_width = width;
                state.mark_durable_edit();
            }
        }
        ui.allocate_ui_with_layout(
            Vec2::new(right_width, size.y),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.set_clip_rect(ui.max_rect());
                panel_frame(ui);
                undertow_mixer(ui, state, focused_track);
            },
        );
    });
}

fn undertow_tools_and_tracks(ui: &mut Ui, state: &mut EditorState) {
    panel_title(
        ui,
        &t(state.language, "Undertow", "アンダートウ"),
        &t(state.language, "Audio studio", "オーディオスタジオ"),
    );
    ui.label(
        RichText::new(t(state.language, "Tools", "ツール"))
            .small()
            .strong(),
    );
    ui.vertical(|ui| {
        for (tool, icon, en, jp, key) in [
            (TimelineTool::Pointer, "↖", "Selection", "選択", "A"),
            (TimelineTool::Range, "▭", "Range", "範囲", "R"),
            (TimelineTool::Trim, "↔", "Trim", "トリム", "T"),
            (TimelineTool::Razor, "✂", "Blade", "ブレード", "B"),
            (
                TimelineTool::DynamicTrim,
                "◖◗",
                "Dynamic trim",
                "ダイナミックトリム",
                "W",
            ),
            (TimelineTool::Slip, "⇄", "Slip", "スリップ", "Y"),
        ] {
            ui.horizontal(|ui| {
                if ui
                    .add_sized(
                        Vec2::splat(30.0),
                        egui::Button::selectable(state.tool == tool, icon),
                    )
                    .on_hover_text(format!("{} ({key})", t(state.language, en, jp)))
                    .clicked()
                {
                    state.tool = tool;
                }
                ui.label(RichText::new(t(state.language, en, jp)).small());
            });
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(t(state.language, "Audio tracks", "オーディオトラック"))
                .small()
                .strong(),
        );
        if ui
            .small_button("+")
            .on_hover_text(t(
                state.language,
                "Add audio track",
                "オーディオトラックを追加",
            ))
            .clicked()
        {
            state.add_timeline_track(TrackKind::Audio);
            if let Some(track_id) = state
                .timeline
                .tracks
                .iter()
                .rev()
                .find(|track| track.kind == TrackKind::Audio)
                .map(|track| track.id)
            {
                state.focus_undertow_track(track_id);
            }
        }
    });
    let tracks = state
        .timeline
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Audio)
        .enumerate()
        .map(|(index, track)| {
            (
                track.id,
                index + 1,
                track.clips.len(),
                track.muted,
                track.solo,
            )
        })
        .collect::<Vec<_>>();
    let mut mute_change = None;
    let mut solo_change = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (track_id, ordinal, clips, muted, solo) in tracks {
            Frame::new()
                .fill(if state.undertow_track == Some(track_id) {
                    Color32::from_rgb(22, 63, 73)
                } else {
                    Color32::from_rgb(17, 24, 31)
                })
                .inner_margin(egui::Margin::symmetric(6, 5))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(
                                state.undertow_track == Some(track_id),
                                format!(
                                    "A{ordinal}  ·  {clips} {}",
                                    t(state.language, "clips", "クリップ")
                                ),
                            )
                            .on_hover_text(t(
                                state.language,
                                "Focus this track in Undertow",
                                "このトラックをアンダートウで表示",
                            ))
                            .clicked()
                        {
                            state.focus_undertow_track(track_id);
                        }
                        if ui
                            .small_button(if muted { "M" } else { "m" })
                            .on_hover_text(t(
                                state.language,
                                if muted { "Unmute track" } else { "Mute track" },
                                if muted {
                                    "ミュート解除"
                                } else {
                                    "ミュート"
                                },
                            ))
                            .clicked()
                        {
                            mute_change = Some((track_id, !muted));
                        }
                        if ui
                            .small_button(if solo { "S" } else { "s" })
                            .on_hover_text(t(
                                state.language,
                                if solo { "Disable solo" } else { "Solo track" },
                                if solo { "ソロを解除" } else { "ソロ" },
                            ))
                            .clicked()
                        {
                            solo_change = Some((track_id, !solo));
                        }
                    });
                });
        }
    });
    if let Some((track_id, muted)) = mute_change {
        let before = state.timeline_history_checkpoint();
        let generation = state.timeline.generation();
        if state.timeline.set_track_muted(track_id, muted).is_ok()
            && state.mark_changed_timeline_generation(generation)
        {
            state.record_timeline_history(before);
        }
    }
    if let Some((track_id, solo)) = solo_change {
        let before = state.timeline_history_checkpoint();
        let generation = state.timeline.generation();
        if state.timeline.set_track_solo(track_id, solo).is_ok()
            && state.mark_changed_timeline_generation(generation)
        {
            state.record_timeline_history(before);
        }
    }
}

fn undertow_transport(ui: &mut Ui, state: &mut EditorState) {
    panel_title(
        ui,
        &t(
            state.language,
            "Audio transport",
            "オーディオトランスポート",
        ),
        &format_timecode_at_frame_rate(state.playhead, state.frame_rate),
    );
    transport_controls(ui, state);
    let meter_rect = ui.allocate_space(Vec2::new(ui.available_width(), 58.0)).1;
    draw_audio_meters(ui.painter(), meter_rect, active_audio_levels(state));
}

fn undertow_mixer(ui: &mut Ui, state: &mut EditorState, focused_track: Option<TrackId>) {
    let track_summary = focused_track
        .and_then(|track_id| state.timeline.track(track_id))
        .map_or("—".to_owned(), |track| {
            format!("{} clips", track.clips.len())
        });
    panel_title(ui, &t(state.language, "Mixer", "ミキサー"), &track_summary);
    ui.label(
        RichText::new(t(state.language, "Live output", "ライブ出力"))
            .small()
            .strong(),
    );
    let meter_rect = ui.allocate_space(Vec2::new(ui.available_width(), 72.0)).1;
    draw_audio_meters(ui.painter(), meter_rect, active_audio_levels(state));
    ui.separator();
    if let Some(track_id) = focused_track.filter(|track_id| {
        state
            .timeline
            .track(*track_id)
            .is_some_and(|track| track.kind == TrackKind::Audio)
    }) {
        let (mut gain_db, mut pan, muted, solo) = state
            .timeline
            .track(track_id)
            .map(|track| (track.gain_db, track.pan, track.muted, track.solo))
            .unwrap_or_default();
        ui.label(RichText::new(t(state.language, "Track", "トラック")).strong());
        ui.horizontal(|ui| {
            if ui
                .selectable_label(muted, "M")
                .on_hover_text(t(state.language, "Mute", "ミュート"))
                .clicked()
            {
                apply_track_header_edit(state, |timeline| {
                    timeline.set_track_muted(track_id, !muted)
                });
            }
            if ui
                .selectable_label(solo, "S")
                .on_hover_text(t(state.language, "Solo", "ソロ"))
                .clicked()
            {
                apply_track_header_edit(state, |timeline| timeline.set_track_solo(track_id, !solo));
            }
        });
        let gain_response = ui.add(
            egui::Slider::new(&mut gain_db, MIN_GAIN_DB..=MAX_GAIN_DB)
                .text(t(state.language, "Gain", "ゲイン"))
                .suffix(" dB"),
        );
        mixer_live_edit(state, &gain_response, |timeline| {
            timeline.set_track_audio_gain(track_id, gain_db)
        });
        let pan_response = ui.add(egui::Slider::new(&mut pan, MIN_PAN..=MAX_PAN).text(t(
            state.language,
            "Pan",
            "パン",
        )));
        mixer_live_edit(state, &pan_response, |timeline| {
            timeline.set_track_pan(track_id, pan)
        });
    } else {
        ui.label(t(
            state.language,
            "Select an audio track to mix gain, pan, mute, and solo.",
            "オーディオトラックを選んでゲイン、パン、ミュート、ソロを調整します。",
        ));
    }
    ui.separator();
    if let Some(clip) = state
        .selected_timeline_clip
        .and_then(|clip_id| state.timeline.clip(clip_id).cloned())
        .filter(|clip| {
            state
                .timeline
                .track(clip.track_id)
                .is_some_and(|track| track.kind == TrackKind::Audio)
        })
    {
        ui.label(RichText::new(t(state.language, "Selected clip", "選択クリップ")).strong());
        ui.monospace(format!("{:.1} dB", clip.gain_db));
        let mut left = clip.gain_left_db;
        let mut right = clip.gain_right_db;
        let left_response = ui.add(
            egui::Slider::new(&mut left, MIN_GAIN_DB..=MAX_GAIN_DB)
                .text(t(state.language, "Left", "左"))
                .suffix(" dB"),
        );
        mixer_live_edit(state, &left_response, |timeline| {
            timeline.set_audio_channel_gain(clip.id, left, right)
        });
        let right_response = ui.add(
            egui::Slider::new(&mut right, MIN_GAIN_DB..=MAX_GAIN_DB)
                .text(t(state.language, "Right", "右"))
                .suffix(" dB"),
        );
        mixer_live_edit(state, &right_response, |timeline| {
            timeline.set_audio_channel_gain(clip.id, left, right)
        });
        ui.label(format!(
            "{}: {}  ·  {}: {}",
            t(state.language, "Fade in", "フェードイン"),
            format_duration(clip.fade_in.duration.0 as f64 / 1_000_000.0),
            t(state.language, "Fade out", "フェードアウト"),
            format_duration(clip.fade_out.duration.0 as f64 / 1_000_000.0),
        ));
    } else {
        ui.label(t(
            state.language,
            "Select an audio clip to inspect its gain and fades.",
            "オーディオクリップを選択してゲインとフェードを確認します。",
        ));
    }
}

fn apply_track_header_edit(
    state: &mut EditorState,
    edit: impl FnOnce(&mut Timeline) -> Result<(), TimelineError>,
) {
    let before = state.timeline_history_checkpoint();
    let generation = state.timeline.generation();
    if edit(&mut state.timeline).is_ok() && state.mark_changed_timeline_generation(generation) {
        state.record_timeline_history(before);
    }
}

fn mixer_live_edit(
    state: &mut EditorState,
    response: &egui::Response,
    edit: impl FnOnce(&mut Timeline) -> Result<(), TimelineError>,
) {
    let discrete = response.changed() && !response.dragged();
    if response.drag_started() || discrete {
        state.begin_timeline_history();
    }
    if response.changed() {
        let generation = state.timeline.generation();
        if edit(&mut state.timeline).is_ok() {
            let _ = state.mark_changed_timeline_generation(generation);
        }
    }
    if response.drag_stopped() || discrete {
        state.commit_timeline_history();
    }
}

fn paint_track_header_chip(
    painter: &egui::Painter,
    rect: Rect,
    label: &str,
    armed: bool,
    hovered: bool,
    armed_fill: Color32,
) {
    painter.rect_filled(
        rect,
        3.0,
        if armed {
            armed_fill
        } else if hovered {
            Color32::from_rgb(46, 61, 72)
        } else {
            Color32::from_rgb(29, 39, 48)
        },
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(10.0),
        if armed {
            Color32::WHITE
        } else {
            Color32::from_rgb(157, 178, 192)
        },
    );
}

/// A subtle frame keeps the sections legible; the neighboring dedicated splitter is the
/// larger, high-contrast grab target so users do not have to hunt for a one-pixel edge.
fn panel_frame(ui: &Ui) {
    ui.painter()
        .rect_filled(ui.max_rect(), 3.0, Color32::from_rgb(14, 18, 24));
    ui.painter().rect_stroke(
        ui.max_rect(),
        3.0,
        Stroke::new(1.0, Color32::from_rgb(42, 54, 66)),
        StrokeKind::Inside,
    );
}

fn vertical_splitter(ui: &mut Ui, height: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(SPLITTER_THICKNESS, height),
        Sense::click_and_drag(),
    );
    let active = response.hovered() || response.dragged();
    let color = if active {
        Color32::from_rgb(72, 150, 190)
    } else {
        Color32::from_rgb(37, 49, 61)
    };
    let painter = ui.painter();
    painter.line_segment(
        [
            Pos2::new(rect.center().x, rect.top() + 5.0),
            Pos2::new(rect.center().x, rect.bottom() - 5.0),
        ],
        Stroke::new(if active { 2.0 } else { 1.0 }, color),
    );
    for y in [-5.0, 0.0, 5.0] {
        painter.circle_filled(Pos2::new(rect.center().x, rect.center().y + y), 1.2, color);
    }
    response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
}

fn horizontal_splitter(ui: &mut Ui, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, SPLITTER_THICKNESS),
        Sense::click_and_drag(),
    );
    let active = response.hovered() || response.dragged();
    let color = if active {
        Color32::from_rgb(72, 150, 190)
    } else {
        Color32::from_rgb(37, 49, 61)
    };
    let painter = ui.painter();
    painter.line_segment(
        [
            Pos2::new(rect.left() + 5.0, rect.center().y),
            Pos2::new(rect.right() - 5.0, rect.center().y),
        ],
        Stroke::new(if active { 2.0 } else { 1.0 }, color),
    );
    for x in [-5.0, 0.0, 5.0] {
        painter.circle_filled(Pos2::new(rect.center().x + x, rect.center().y), 1.2, color);
    }
    response.on_hover_cursor(egui::CursorIcon::ResizeVertical)
}

fn panel_title(ui: &mut Ui, title: &str, subtitle: &str) {
    Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(subtitle)
                                .small()
                                .color(Color32::from_rgb(130, 154, 171)),
                        )
                        .truncate(),
                    )
                    .on_hover_text(subtitle);
                });
            });
        });
    ui.separator();
}

fn claim_media_press_from_previous_layout(ui: &Ui, state: &mut EditorState) {
    let pressed_media = ui.input(|input| {
        if !input.pointer.primary_pressed() {
            return None;
        }
        let origin = input.pointer.press_origin()?;
        state
            .media_drag_rects
            .iter()
            .find_map(|(&media_id, rect)| rect.contains(origin).then_some(media_id))
    });
    // Rows are rebuilt below. Keeping only current-frame geometry avoids stale hit targets when
    // the search filter or panel size changes between frames.
    state.media_drag_rects.clear();
    if let Some(media_id) = pressed_media {
        state.active_media_drag = Some(media_id);
    }
}

fn media_pool(ui: &mut Ui, state: &mut EditorState) {
    panel_title(
        ui,
        &t(state.language, "Media Pool", "メディアプール"),
        &format!("{}", state.media.len()),
    );
    ui.horizontal(|ui| {
        if ui
            .button(t(state.language, "Open Media", "メディアを開く"))
            .clicked()
        {
            state.emit(EditorAction::ChooseMediaFiles);
        }
        let can_add = state
            .selected()
            .is_some_and(|item| media_kind_can_place(item.kind));
        if ui
            .add_enabled(
                can_add,
                egui::Button::new(t(state.language, "Add to Timeline", "タイムラインに追加")),
            )
            .on_hover_text(t(
                state.language,
                "Place selected media at the start of the default tracks",
                "選択したメディアを既定トラックの先頭に配置",
            ))
            .clicked()
        {
            state.add_selected_to_timeline();
        }
        if ui
            .button(t(state.language, "Import", "読み込む"))
            .on_hover_text(t(
                state.language,
                "Choose media files to import",
                "読み込むメディアを選択",
            ))
            .clicked()
        {
            state.emit(EditorAction::ChooseMediaFiles);
        }
    });
    let search_changed = ui
        .add(egui::TextEdit::singleline(&mut state.search).hint_text(t(
            state.language,
            "Search media",
            "メディアを検索",
        )))
        .changed();
    if search_changed {
        state.refresh_media_filter();
    }
    ui.add_space(4.0);
    if state.filtered_media.is_empty() {
        let desired = Vec2::new(ui.available_width(), 140.0);
        let (rect, response) = ui.allocate_exact_size(desired, Sense::click());
        ui.painter().rect(
            rect,
            6.0,
            Color32::from_rgb(15, 23, 31),
            Stroke::new(1.0, Color32::from_rgb(53, 89, 110)),
            StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            t(
                state.language,
                "Drop media here\nor choose Import",
                "メディアをここにドロップ\nまたは「読み込む」を選択",
            ),
            FontId::proportional(13.0),
            Color32::from_rgb(157, 183, 200),
        );
        if response.clicked() {
            state.emit(EditorAction::ChooseMediaFiles);
        }
    } else {
        let mut add_to_timeline = None;
        egui::ScrollArea::vertical().show_rows(ui, 58.0, state.filtered_media.len(), |ui, rows| {
            for row in rows {
                let media_card_width = ui.available_width();
                let item = &state.media[state.filtered_media[row]];
                let item_id = item.id;
                let kind = item.kind;
                let label = item.label.clone();
                let selected = state.selected_media == Some(item_id);
                let strip = state.video_strips.get(&item_id).copied();
                let analysis_ready = strip.is_some()
                    || state.waveforms.contains_key(&item_id)
                    || state.waveform_errors.contains_key(&item_id);
                let row = ui.scope(|ui| {
                    // The whole visible card is the drag source, including its thumbnail and
                    // trailing empty space. Without these minimum bounds the scope response can
                    // collapse to the label column, making the most obvious grab target inert.
                    ui.set_min_width(media_card_width);
                    ui.set_min_height(58.0);
                    ui.horizontal(|ui| {
                        if matches!(kind, MediaKind::Video | MediaKind::Image) {
                            media_thumbnail(ui, strip, state.language);
                        } else {
                            ui.allocate_exact_size(Vec2::new(72.0, 40.5), Sense::hover());
                        }
                        ui.vertical(|ui| {
                            ui.label(label);
                            ui.label(
                                RichText::new(t(
                                    state.language,
                                    if analysis_ready {
                                        "Ready"
                                    } else {
                                        "Awaiting timeline analysis"
                                    },
                                    if analysis_ready {
                                        "準備完了"
                                    } else {
                                        "タイムライン解析待ち"
                                    },
                                ))
                                .small()
                                .color(Color32::from_rgb(124, 151, 168)),
                            );
                        });
                    });
                });
                // One widget owns selection, context menus and drag-and-drop. Calling
                // `Response::interact` on `dnd_drag_source`'s union response is unsupported by
                // egui and could leave the row looking draggable without ever setting a payload.
                let response = ui
                    .interact(
                        row.response.rect,
                        ui.make_persistent_id(("media-pool-drag", item_id)),
                        Sense::click_and_drag(),
                    )
                    .on_hover_cursor(egui::CursorIcon::Grab);
                state.media_drag_rects.insert(item_id, response.rect);
                response.dnd_set_drag_payload(MediaDragPayload { media_id: item_id });
                // Claim the media as soon as the pointer goes down. A vertical ScrollArea can
                // win egui's drag gesture before `drag_started` reaches this row. The thumbnail
                // and labels can also be the top-most hovered widgets, so inspect the physical
                // press origin instead of requiring this overlay response to own the click.
                let pressed_inside_row = ui.input(|input| {
                    input.pointer.primary_pressed()
                        && input
                            .pointer
                            .press_origin()
                            .is_some_and(|point| response.rect.contains(point))
                });
                if pressed_inside_row
                    || response.is_pointer_button_down_on()
                    || response.drag_started()
                    || response.dragged()
                {
                    state.active_media_drag = Some(item_id);
                }
                if selected {
                    ui.painter().rect_stroke(
                        response.rect,
                        3.0,
                        Stroke::new(1.0, Color32::from_rgb(79, 164, 207)),
                        StrokeKind::Inside,
                    );
                }
                if response.clicked() && state.selected_media != Some(item_id) {
                    state.selected_media = Some(item_id);
                    state.mark_durable_edit();
                }
                if response.double_clicked() {
                    if state.selected_media != Some(item_id) {
                        state.selected_media = Some(item_id);
                        state.mark_durable_edit();
                    }
                    add_to_timeline = Some(item_id);
                }
                response.context_menu(|ui| {
                    if ui
                        .add_enabled(
                            media_kind_can_place(kind),
                            egui::Button::new(t(
                                state.language,
                                "Add to Timeline",
                                "タイムラインに追加",
                            )),
                        )
                        .clicked()
                    {
                        add_to_timeline = Some(item_id);
                        ui.close();
                    }
                });
            }
        });
        if let Some(id) = add_to_timeline {
            if state.selected_media != Some(id) {
                state.selected_media = Some(id);
                state.mark_durable_edit();
            }
            state.add_selected_to_timeline();
        }
    }
    if state.drop_hovered {
        let rect = ui.max_rect();
        ui.painter()
            .rect_filled(rect, 4.0, Color32::from_rgba_unmultiplied(21, 105, 146, 54));
        ui.painter().rect_stroke(
            rect.shrink(3.0),
            4.0,
            Stroke::new(2.0, Color32::from_rgb(101, 202, 239)),
            StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            t(
                state.language,
                "Release to import media",
                "離してメディアを読み込む",
            ),
            FontId::proportional(15.0),
            Color32::WHITE,
        );
    }
}

fn media_kind_can_place(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Video | MediaKind::Audio | MediaKind::Image)
}

fn media_thumbnail(ui: &mut Ui, strip: Option<CachedVideoStrip>, language: Language) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(72.0, 40.5), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, Color32::from_rgb(29, 39, 49));
    if let Some(strip) = strip {
        draw_video_strip_frame(painter, rect, strip, 0);
    } else {
        painter.line_segment(
            [rect.left_top(), rect.right_bottom()],
            Stroke::new(1.0, Color32::from_rgb(79, 96, 109)),
        );
        painter.line_segment(
            [rect.right_top(), rect.left_bottom()],
            Stroke::new(1.0, Color32::from_rgb(79, 96, 109)),
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            t(language, "No preview", "プレビューなし"),
            FontId::proportional(8.0),
            Color32::from_rgb(157, 183, 200),
        );
    }
    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, Color32::from_rgb(65, 83, 96)),
        StrokeKind::Inside,
    );
}

fn monitor_frame_matches_target(frame: MonitorFrame, target: Option<(MediaId, Tick, f32)>) -> bool {
    let Some((media_id, _, _)) = target else {
        return false;
    };
    frame
        .media_id
        .is_none_or(|frame_media| frame_media == media_id)
}

#[cfg(test)]
fn viewer(ui: &mut Ui, state: &mut EditorState) {
    let mut canvas = EguiViewerCanvas::default();
    viewer_with_canvas(ui, state, &mut canvas);
}

fn viewer_with_canvas(ui: &mut Ui, state: &mut EditorState, viewer: &mut dyn ViewerCanvas) {
    viewer_header(ui, state);
    let max = ui.available_size();
    let project_size = state.project_canvas_size();
    let project_aspect = project_size.0 as f32 / project_size.1 as f32;
    let desired_h = (max.x / project_aspect).min((max.y - 40.0).max(90.0));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(max.x, desired_h), Sense::hover());
    let canvas = fit_aspect(rect.shrink(1.0), project_aspect);
    state.update_monitor_decode_size(canvas.size(), ui.ctx().pixels_per_point());
    ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
    ui.painter().rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, Color32::from_rgb(42, 58, 69)),
        StrokeKind::Inside,
    );
    let mut request_layers = [None; MAX_COMPOSITE_LAYERS];
    let mut frames = [None; MAX_COMPOSITE_LAYERS];
    let mut effects = [EvaluatedVideoEffectStack::default(); MAX_COMPOSITE_LAYERS];
    let mut black_mattes_before = [0.0; MAX_COMPOSITE_LAYERS];
    let mut black_mattes_after = [0.0; MAX_COMPOSITE_LAYERS];
    let mut white_mattes_before = [0.0; MAX_COMPOSITE_LAYERS];
    let mut white_mattes_after = [0.0; MAX_COMPOSITE_LAYERS];
    let mut transition_reveals = [None; MAX_COMPOSITE_LAYERS];
    let mut transition_offsets = [(0.0, 0.0); MAX_COMPOSITE_LAYERS];
    let mut has_viewer_target = false;
    for (layer, target) in state.playback_targets().enumerate() {
        has_viewer_target = true;
        black_mattes_before[layer] = target.black_matte_before;
        black_mattes_after[layer] = target.black_matte_after;
        white_mattes_before[layer] = target.white_matte_before;
        white_mattes_after[layer] = target.white_matte_after;
        transition_reveals[layer] = target.transition_reveal;
        transition_offsets[layer] = target.transition_offset;
        let frame = state.monitor_frame_for_layer(layer).filter(|frame| {
            monitor_frame_matches_target(
                *frame,
                Some((target.media_id, target.source_tick, target.opacity)),
            )
        });
        let source_size = target
            .source_size
            .or_else(|| frame.map(|frame| (frame.width, frame.height)));
        let Some((width, height)) = source_size else {
            continue;
        };
        request_layers[layer] = Some(CompositeLayerInput {
            clip_id: target.clip_id,
            source_size: PixelSize::new(width, height),
            transform: target.transform,
            fade_opacity: target.opacity,
        });
        frames[layer] = frame;
        effects[layer] = target.video_effects;
    }
    let plan = plan_composition(CompositionRequest {
        project_size: PixelSize::new(project_size.0, project_size.1),
        layers: request_layers,
    });
    viewer.begin(ui, canvas, PixelSize::new(project_size.0, project_size.1));
    let mut painted_frame = false;
    if let Some(plan) = plan {
        for layer in 0..MAX_COMPOSITE_LAYERS {
            if black_mattes_before[layer] > 0.0 {
                viewer.black_matte(black_mattes_before[layer]);
                painted_frame = true;
            }
            if white_mattes_before[layer] > 0.0 {
                viewer.white_matte(white_mattes_before[layer]);
                painted_frame = true;
            }
            let (Some(mut quad), Some(frame), Some(input)) =
                (plan.layers[layer], frames[layer], request_layers[layer])
            else {
                if black_mattes_after[layer] > 0.0 {
                    viewer.black_matte(black_mattes_after[layer]);
                    painted_frame = true;
                }
                if white_mattes_after[layer] > 0.0 {
                    viewer.white_matte(white_mattes_after[layer]);
                    painted_frame = true;
                }
                continue;
            };
            apply_transition_geometry(
                &mut quad,
                transition_reveals[layer],
                transition_offsets[layer],
                PixelSize::new(project_size.0, project_size.1),
            );
            let content_uv = decoded_content_uv_rect(
                input.source_size,
                PixelSize::new(frame.width, frame.height),
            );
            viewer.layer(layer, frame, content_uv, quad, effects[layer]);
            painted_frame = true;
            if black_mattes_after[layer] > 0.0 {
                viewer.black_matte(black_mattes_after[layer]);
            }
            if white_mattes_after[layer] > 0.0 {
                viewer.white_matte(white_mattes_after[layer]);
            }
        }
    }
    if !painted_frame && matches!(state.monitor_status, MonitorStatus::Error(_)) {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            t(
                state.language,
                "Preview unavailable",
                "プレビューを表示できません",
            ),
            FontId::proportional(15.0),
            Color32::from_rgb(143, 160, 175),
        );
    } else if !has_viewer_target && state.selected().is_none() {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            t(
                state.language,
                "Import media to preview it",
                "メディアを読み込むとプレビューできます",
            ),
            FontId::proportional(15.0),
            Color32::from_rgb(143, 160, 175),
        );
    }
    paint_active_titles(ui, state, canvas, project_size);
    if let MonitorStatus::Error(error) = &state.monitor_status {
        ui.painter().text(
            rect.left_bottom() + Vec2::new(9.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            error,
            FontId::proportional(10.0),
            Color32::from_rgb(222, 150, 145),
        );
    }
    transport_controls(ui, state);
}

fn paint_active_titles(
    ui: &mut Ui,
    state: &mut EditorState,
    canvas: Rect,
    project_size: (u32, u32),
) {
    let titles = state
        .timeline
        .active_titles(state.playhead)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let existing = state
        .timeline
        .titles()
        .iter()
        .map(|title| title.id)
        .collect::<HashSet<_>>();
    state.title_textures.retain(|id, _| existing.contains(id));
    for title in titles {
        let local_tick = Tick(state.playhead.0 - title.start.0);
        let opacity = nle_title::title_fade_opacity(&title, local_tick);
        if opacity <= 0.0 {
            continue;
        }
        let refresh = state
            .title_textures
            .get(&title.id)
            .is_none_or(|cached| cached.title != title);
        if refresh {
            let Ok(raster) = nle_title::rasterize_title(&title) else {
                continue;
            };
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [raster.width as usize, raster.height as usize],
                raster.rgba.as_ref(),
            );
            let texture = ui.ctx().load_texture(
                format!("title-{}", title.id.0),
                image,
                egui::TextureOptions::LINEAR,
            );
            state.title_textures.insert(
                title.id,
                CachedTitleTexture {
                    title: title.clone(),
                    texture,
                    size: [raster.width as usize, raster.height as usize],
                },
            );
        }
        let Some(cached) = state.title_textures.get(&title.id) else {
            continue;
        };
        let scale_x = canvas.width() / project_size.0 as f32;
        let scale_y = canvas.height() / project_size.1 as f32;
        let size = Vec2::new(
            cached.size[0] as f32 * scale_x,
            cached.size[1] as f32 * scale_y,
        );
        let center = Pos2::new(
            canvas.left() + title.position_x * canvas.width(),
            canvas.top() + title.position_y * canvas.height(),
        );
        let rect = Rect::from_center_size(center, size);
        ui.painter().image(
            cached.texture.id(),
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::from_white_alpha((opacity * 255.0).round() as u8),
        );
        if state.selected_title == Some(title.id) {
            ui.painter().rect_stroke(
                rect.expand(3.0),
                2.0,
                Stroke::new(1.0, Color32::from_rgb(111, 213, 245)),
                StrokeKind::Outside,
            );
        }
    }
}

/// Paints a compositor quad after mapping project pixels into the fitted monitor canvas.  Decoded
/// textures can be letterboxed to a requested preview size, so compositor source UVs are remapped
/// into their actual content rect instead of sampling bars as picture data.
fn paint_composite_quad(
    painter: &egui::Painter,
    canvas: Rect,
    project_size: PixelSize,
    frame: MonitorFrame,
    content_uv: Rect,
    quad: nle_compositor::CompositeQuad,
) {
    let mut mesh = egui::Mesh {
        texture_id: frame.texture,
        ..Default::default()
    };
    let tint = Color32::from_white_alpha((quad.opacity * 255.0).round() as u8);
    for (position, uv) in quad.positions.into_iter().zip(quad.uvs) {
        let screen = Pos2::new(
            canvas.left() + position.x / project_size.width as f32 * canvas.width(),
            canvas.top() + position.y / project_size.height as f32 * canvas.height(),
        );
        mesh.vertices.push(egui::epaint::Vertex {
            pos: screen,
            uv: Pos2::new(
                content_uv.left() + uv.u * content_uv.width(),
                content_uv.top() + uv.v * content_uv.height(),
            ),
            color: tint,
        });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

fn decoded_content_uv_rect(source: PixelSize, frame: PixelSize) -> Rect {
    let source_aspect = source.width as f32 / source.height as f32;
    let frame_aspect = frame.width as f32 / frame.height as f32;
    if frame_aspect > source_aspect {
        let width = source_aspect / frame_aspect;
        Rect::from_center_size(Pos2::new(0.5, 0.5), Vec2::new(width, 1.0))
    } else {
        let height = frame_aspect / source_aspect;
        Rect::from_center_size(Pos2::new(0.5, 0.5), Vec2::new(1.0, height))
    }
}

/// Applies the monitor-only geometry used by wipe and slide transitions.  This intentionally
/// operates on the compositor's final quad, so the native renderer and egui fallback consume
/// identical vertices and UVs without a renderer-specific transition path.
fn apply_transition_geometry(
    quad: &mut CompositeQuad,
    reveal: Option<TransitionReveal>,
    offset: (f32, f32),
    project_size: PixelSize,
) {
    if let Some(reveal) = reveal {
        let progress = offset.0.clamp(0.0, 1.0);
        match reveal {
            TransitionReveal::FromLeft => {
                interpolate_quad_vertex(quad, 1, 0, progress);
                interpolate_quad_vertex(quad, 2, 3, progress);
            }
            TransitionReveal::FromRight => {
                interpolate_quad_vertex(quad, 0, 1, progress);
                interpolate_quad_vertex(quad, 3, 2, progress);
            }
            TransitionReveal::FromTop => {
                interpolate_quad_vertex(quad, 2, 1, progress);
                interpolate_quad_vertex(quad, 3, 0, progress);
            }
            TransitionReveal::FromBottom => {
                interpolate_quad_vertex(quad, 0, 3, progress);
                interpolate_quad_vertex(quad, 1, 2, progress);
            }
        }
        return;
    }
    let dx = offset.0.clamp(-1.0, 1.0) * project_size.width as f32;
    let dy = offset.1.clamp(-1.0, 1.0) * project_size.height as f32;
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    for position in &mut quad.positions {
        position.x += dx;
        position.y += dy;
    }
}

fn interpolate_quad_vertex(quad: &mut CompositeQuad, destination: usize, source: usize, t: f32) {
    let destination_position = quad.positions[destination];
    let source_position = quad.positions[source];
    quad.positions[destination].x =
        source_position.x + (destination_position.x - source_position.x) * t;
    quad.positions[destination].y =
        source_position.y + (destination_position.y - source_position.y) * t;
    let destination_uv = quad.uvs[destination];
    let source_uv = quad.uvs[source];
    quad.uvs[destination].u = source_uv.u + (destination_uv.u - source_uv.u) * t;
    quad.uvs[destination].v = source_uv.v + (destination_uv.v - source_uv.v) * t;
}

fn viewer_header(ui: &mut Ui, state: &mut EditorState) {
    let language = state.language;
    let project_size = state.project_canvas_size();
    ui.horizontal(|ui| {
        ui.label(RichText::new(t(language, "Viewer", "ビューアー")).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut selected = state.preview_quality();
            let response = egui::ComboBox::from_id_salt("viewer-preview-quality")
                .width(108.0)
                .selected_text(preview_quality_display(
                    language,
                    selected,
                    state.resolved_preview_quality(),
                ))
                .show_ui(ui, |ui| {
                    for quality in [
                        PreviewQuality::Auto,
                        PreviewQuality::Full,
                        PreviewQuality::Half,
                        PreviewQuality::Quarter,
                        PreviewQuality::Eighth,
                    ] {
                        let label = preview_quality_option_label(language, quality);
                        ui.selectable_value(&mut selected, quality, label)
                            .on_hover_text(preview_quality_tooltip(language, quality));
                    }
                })
                .response;
            response.on_hover_text(t(
                language,
                "Preview quality changes live decoding only; export stays full resolution.",
                "プレビュー品質はライブデコードのみに適用され、書き出し解像度は変わりません。",
            ));
            if selected != state.preview_quality() {
                state.set_preview_quality(selected);
            }
            ui.label(
                RichText::new(project_aspect_label(project_size))
                    .small()
                    .color(Color32::from_rgb(130, 154, 171)),
            )
            .on_hover_text(t(
                language,
                &format!("Project frame: {} × {}", project_size.0, project_size.1),
                &format!("プロジェクト画面: {} × {}", project_size.0, project_size.1),
            ));
        });
    });
    ui.separator();
}

fn project_aspect_label((width, height): (u32, u32)) -> String {
    let divisor = greatest_common_divisor(width.max(1), height.max(1));
    format!("{}:{}", width.max(1) / divisor, height.max(1) / divisor)
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

fn preview_quality_display(
    language: Language,
    selected: PreviewQuality,
    resolved: PreviewQuality,
) -> &'static str {
    match selected {
        PreviewQuality::Auto => match (language, resolved) {
            (Language::English, PreviewQuality::Auto | PreviewQuality::Full) => "Auto · 1/1",
            (Language::Japanese, PreviewQuality::Auto | PreviewQuality::Full) => "自動 · 1/1",
            (Language::English, PreviewQuality::Half) => "Auto · 1/2",
            (Language::Japanese, PreviewQuality::Half) => "自動 · 1/2",
            (Language::English, PreviewQuality::Quarter) => "Auto · 1/4",
            (Language::Japanese, PreviewQuality::Quarter) => "自動 · 1/4",
            (Language::English, PreviewQuality::Eighth) => "Auto · 1/8",
            (Language::Japanese, PreviewQuality::Eighth) => "自動 · 1/8",
        },
        quality => preview_quality_option_label(language, quality),
    }
}

fn preview_quality_option_label(language: Language, quality: PreviewQuality) -> &'static str {
    match (language, quality) {
        (Language::English, PreviewQuality::Auto) => "Auto",
        (Language::Japanese, PreviewQuality::Auto) => "自動",
        (Language::English, PreviewQuality::Full) => "Full · 1/1",
        (Language::Japanese, PreviewQuality::Full) => "フル · 1/1",
        (Language::English, PreviewQuality::Half) => "Half · 1/2",
        (Language::Japanese, PreviewQuality::Half) => "半分 · 1/2",
        (Language::English, PreviewQuality::Quarter) => "Quarter · 1/4",
        (Language::Japanese, PreviewQuality::Quarter) => "1/4 · 1/4",
        (Language::English, PreviewQuality::Eighth) => "Eighth · 1/8",
        (Language::Japanese, PreviewQuality::Eighth) => "1/8 · 1/8",
    }
}

fn preview_quality_tooltip(language: Language, quality: PreviewQuality) -> &'static str {
    match (language, quality) {
        (Language::English, PreviewQuality::Auto) => {
            "Adapts preview resolution for smooth playback."
        }
        (Language::Japanese, PreviewQuality::Auto) => {
            "滑らかな再生のためにプレビュー解像度を自動調整します。"
        }
        (Language::English, PreviewQuality::Full) => "Decode at the viewer's full resolution.",
        (Language::Japanese, PreviewQuality::Full) => "ビューアーのフル解像度でデコードします。",
        (Language::English, PreviewQuality::Half) => {
            "Decode at half resolution for faster playback."
        }
        (Language::Japanese, PreviewQuality::Half) => {
            "より高速な再生のため半分の解像度でデコードします。"
        }
        (Language::English, PreviewQuality::Quarter) => {
            "Decode at quarter resolution for demanding timelines."
        }
        (Language::Japanese, PreviewQuality::Quarter) => {
            "負荷の高いタイムライン向けに1/4解像度でデコードします。"
        }
        (Language::English, PreviewQuality::Eighth) => {
            "Decode at one eighth resolution for maximum responsiveness."
        }
        (Language::Japanese, PreviewQuality::Eighth) => {
            "最大限の応答性のため1/8解像度でデコードします。"
        }
    }
}

fn transport_controls(ui: &mut Ui, state: &mut EditorState) {
    const TRANSPORT_WIDTH: f32 = 286.0;
    let transport_space = ((ui.available_width() - TRANSPORT_WIDTH) * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(transport_space);
        ui.spacing_mut().item_spacing.x = 3.0;
        if transport_button(ui, "│◀", t(state.language, "Start", "先頭"), false).clicked() {
            state.set_playhead(Tick(0));
        }
        if transport_button(
            ui,
            "◀",
            t(state.language, "Previous frame", "前のフレーム"),
            false,
        )
        .clicked()
        {
            state.previous_frame();
        }
        if transport_button(
            ui,
            if state.playing { "Ⅱ" } else { "▶" },
            t(state.language, "Play / pause", "再生 / 一時停止"),
            true,
        )
        .clicked()
        {
            state.toggle_playback();
        }
        if transport_button(
            ui,
            "▶",
            t(state.language, "Next frame", "次のフレーム"),
            false,
        )
        .clicked()
        {
            state.next_frame();
        }
        if transport_button(ui, "▶│", t(state.language, "End", "終端"), false).clicked() {
            state.set_playhead(state.timeline_end());
        }
        ui.add_space(9.0);
        let timecode = format_timecode_at_frame_rate(state.playhead, state.frame_rate);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(78.0, 28.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, 4.0, Color32::from_rgb(8, 12, 17));
        ui.painter().rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0, Color32::from_rgb(42, 58, 69)),
            StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            timecode,
            FontId::monospace(10.0),
            Color32::from_rgb(202, 218, 228),
        );
    });
}

fn transport_button(ui: &mut Ui, icon: &str, tooltip: String, primary: bool) -> egui::Response {
    let size = if primary {
        Vec2::new(38.0, 30.0)
    } else {
        Vec2::new(31.0, 28.0)
    };
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if response.is_pointer_button_down_on() {
        Color32::from_rgb(29, 105, 137)
    } else if response.hovered() {
        Color32::from_rgb(28, 48, 61)
    } else if primary {
        Color32::from_rgb(20, 66, 86)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 4.0, fill);
    if primary {
        ui.painter().rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0, Color32::from_rgb(77, 157, 191)),
            StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        icon,
        FontId::proportional(if primary { 15.0 } else { 13.0 }),
        Color32::from_rgb(218, 229, 237),
    );
    response.on_hover_text(tooltip)
}

fn active_audio_levels(state: &EditorState) -> Option<((f32, f32), usize)> {
    let targets = state.audio_playback_targets();
    if targets.is_empty() {
        return None;
    }
    let channels = targets
        .iter()
        .filter_map(|target| state.waveforms.get(&target.media_id)?.channels)
        .max()
        .unwrap_or(2)
        .clamp(1, 2);
    let levels = if state.playing {
        state.audio_meter_levels
    } else {
        (0.0, 0.0)
    };
    Some((levels, channels))
}

fn sanitized_meter_level(value: f32) -> f32 {
    if value.is_finite() {
        value.abs().clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn draw_audio_meters(painter: &egui::Painter, rect: Rect, level: Option<((f32, f32), usize)>) {
    painter.rect_filled(rect, 2.0, Color32::from_rgb(15, 20, 27));
    let ((left, right), channels) = level.unwrap_or(((0.0, 0.0), 2));
    let row_height = 18.0;
    for channel in 0..channels {
        let level = if channel == 0 { left } else { right };
        let db = if level > 0.0 {
            (20.0 * level.log10()).clamp(-60.0, 0.0)
        } else {
            -60.0
        };
        let normalized = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let top = rect.top() + 8.0 + channel as f32 * 25.0;
        let label = if channels == 1 {
            "M"
        } else if channel == 0 {
            "L"
        } else {
            "R"
        };
        painter.text(
            Pos2::new(rect.left() + 5.0, top + row_height * 0.5),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::monospace(10.0),
            Color32::from_rgb(157, 178, 192),
        );
        let bar = Rect::from_min_max(
            Pos2::new(rect.left() + 20.0, top),
            Pos2::new(rect.right() - 42.0, top + row_height),
        );
        painter.rect_filled(bar, 2.0, Color32::from_rgb(29, 39, 47));
        let fill = Rect::from_min_max(
            bar.min,
            Pos2::new(bar.left() + bar.width() * normalized, bar.bottom()),
        );
        let color = if db > -6.0 {
            Color32::from_rgb(238, 91, 73)
        } else if db > -18.0 {
            Color32::from_rgb(223, 185, 65)
        } else {
            Color32::from_rgb(63, 203, 118)
        };
        painter.rect_filled(fill, 2.0, color);
        for threshold in [-18.0_f32, -6.0] {
            let x = bar.left() + bar.width() * ((threshold + 60.0) / 60.0);
            painter.line_segment(
                [Pos2::new(x, bar.top()), Pos2::new(x, bar.bottom())],
                Stroke::new(1.0, Color32::from_white_alpha(90)),
            );
        }
        painter.text(
            Pos2::new(rect.right() - 5.0, top + row_height * 0.5),
            egui::Align2::RIGHT_CENTER,
            if level > 0.0 {
                format!("{db:.1}")
            } else {
                "−∞".to_owned()
            },
            FontId::monospace(9.0),
            Color32::from_rgb(157, 178, 192),
        );
    }
}

fn format_duration(seconds: f64) -> String {
    let total_millis = (seconds.max(0.0) * 1_000.0).round() as u64;
    let minutes = total_millis / 60_000;
    let seconds = (total_millis / 1_000) % 60;
    let millis = total_millis % 1_000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

fn format_file_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_bit_rate(bits_per_second: u64) -> String {
    if bits_per_second >= 1_000_000 {
        format!("{:.2} Mb/s", bits_per_second as f64 / 1_000_000.0)
    } else {
        format!("{:.0} kb/s", bits_per_second as f64 / 1_000.0)
    }
}

fn format_video_metadata(metadata: Option<&MediaMetadata>) -> String {
    let Some(metadata) = metadata else {
        return "—".into();
    };
    match (
        metadata.video_codec.as_deref(),
        metadata.width,
        metadata.height,
    ) {
        (Some(codec), Some(width), Some(height)) => {
            format!("{} · {width}×{height}", codec.to_uppercase())
        }
        (Some(codec), _, _) => codec.to_uppercase(),
        _ => "—".into(),
    }
}

fn format_audio_metadata(
    metadata: Option<&MediaMetadata>,
    waveform: Option<&CachedWaveform>,
) -> String {
    let codec = metadata.and_then(|value| value.audio_codec.as_deref());
    let sample_rate = metadata
        .and_then(|value| value.sample_rate)
        .or_else(|| waveform.and_then(|value| value.sample_rate));
    let channels = metadata
        .and_then(|value| value.channels)
        .or_else(|| waveform.and_then(|value| value.channels));
    let mut parts = Vec::new();
    if let Some(codec) = codec {
        parts.push(codec.to_uppercase());
    }
    if let Some(sample_rate) = sample_rate {
        parts.push(format!("{:.1} kHz", sample_rate as f32 / 1_000.0));
    }
    if let Some(channels) = channels {
        parts.push(match channels {
            1 => "Mono".into(),
            2 => "Stereo".into(),
            _ => format!("{channels} ch"),
        });
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" · ")
    }
}

fn format_stream_metadata(stream: &MediaStreamMetadata) -> String {
    let mut parts = Vec::with_capacity(6);
    if let Some(kind) = stream.kind.as_deref() {
        parts.push(kind.to_owned());
    }
    if let Some(codec) = stream.codec.as_deref() {
        parts.push(codec.to_uppercase());
    }
    if let (Some(width), Some(height)) = (stream.width, stream.height) {
        parts.push(format!("{width}×{height}"));
    }
    if let Some(frame_rate) = stream.frame_rate {
        parts.push(format!("{frame_rate:.3} fps"));
    }
    if let Some(sample_rate) = stream.sample_rate {
        parts.push(format!("{:.1} kHz", sample_rate as f64 / 1_000.0));
    }
    if let Some(channels) = stream.channels {
        parts.push(format!("{channels} ch"));
    }
    if let Some(start) = stream.start_seconds {
        parts.push(format!("start {start:.3} s"));
    }
    if let Some(duration) = stream.duration_seconds {
        parts.push(format!("duration {duration:.3} s"));
    }
    if let Some(time_base) = stream.time_base.as_deref() {
        parts.push(format!("time base {time_base}"));
    }
    if let Some(bit_rate) = stream.bit_rate {
        parts.push(format_bit_rate(bit_rate));
    }
    if parts.is_empty() {
        "—".to_owned()
    } else {
        parts.join(" · ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RightSidebarTab {
    Inspector,
    Audio,
    Color,
    Effects,
    Media,
}

impl RightSidebarTab {
    const ALL: [Self; 5] = [
        Self::Inspector,
        Self::Audio,
        Self::Color,
        Self::Effects,
        Self::Media,
    ];

    fn label(self, language: Language) -> &'static str {
        match (language, self) {
            (Language::English, Self::Inspector) => "Inspect",
            (Language::Japanese, Self::Inspector) => "インスペクタ",
            (Language::English, Self::Audio) => "Audio",
            (Language::Japanese, Self::Audio) => "音声",
            (Language::English, Self::Color) => "Color",
            (Language::Japanese, Self::Color) => "カラー",
            (Language::English, Self::Effects) => "Effects",
            (Language::Japanese, Self::Effects) => "エフェクト",
            (Language::English, Self::Media) => "Media",
            (Language::Japanese, Self::Media) => "情報",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Inspector => "◇",
            Self::Audio => "♫",
            Self::Color => "◉",
            Self::Effects => "FX",
            Self::Media => "ⓘ",
        }
    }

    fn tooltip(self, language: Language) -> &'static str {
        match (language, self) {
            (Language::English, Self::Inspector) => "Inspector",
            (Language::Japanese, Self::Inspector) => "インスペクタ",
            (Language::English, Self::Audio) => "Audio and analysis",
            (Language::Japanese, Self::Audio) => "音声と解析",
            (Language::English, Self::Color) => "Color correction",
            (Language::Japanese, Self::Color) => "カラー補正",
            (Language::English, Self::Effects) => "Effects, video transitions, and settings",
            (Language::Japanese, Self::Effects) => "エフェクト、ビデオトランジション、設定",
            (Language::English, Self::Media) => "Media metadata",
            (Language::Japanese, Self::Media) => "メディア情報",
        }
    }

    fn scroll_id(self) -> egui::Id {
        egui::Id::new(("details-panel-scroll", self))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnalysisPanelState {
    NoSelection,
    AwaitingPlacement,
    Analyzing,
    Ready,
    Offline,
}

fn analysis_panel_state(state: &EditorState, selected_id: Option<MediaId>) -> AnalysisPanelState {
    let Some(media_id) = selected_id else {
        return AnalysisPanelState::NoSelection;
    };
    if state.media_errors.contains_key(&media_id) {
        return AnalysisPanelState::Offline;
    }
    if state.media_metadata.contains_key(&media_id)
        && (state.waveforms.contains_key(&media_id)
            || state.waveform_errors.contains_key(&media_id))
    {
        return AnalysisPanelState::Ready;
    }
    if state
        .timeline
        .tracks
        .iter()
        .any(|track| track.clips.iter().any(|clip| clip.media.0 == media_id))
    {
        AnalysisPanelState::Analyzing
    } else {
        AnalysisPanelState::AwaitingPlacement
    }
}

fn analysis_panel_status(
    language: Language,
    panel_state: AnalysisPanelState,
    playing: bool,
) -> String {
    match panel_state {
        AnalysisPanelState::NoSelection => t(language, "No selection", "未選択"),
        AnalysisPanelState::AwaitingPlacement => t(language, "Not analyzed", "未解析"),
        AnalysisPanelState::Analyzing => t(language, "Analyzing", "解析中"),
        AnalysisPanelState::Ready if playing => t(language, "Live", "ライブ"),
        AnalysisPanelState::Ready => t(language, "Ready", "準備完了"),
        AnalysisPanelState::Offline => t(language, "Offline", "オフライン"),
    }
}

fn title_live_edit(state: &mut EditorState, response: &egui::Response, title: TitleOverlay) {
    let discrete = response.changed() && !response.dragged();
    if response.drag_started() || discrete {
        state.begin_timeline_history();
    }
    if response.changed() && state.timeline.replace_title(title.id, title).is_ok() {
        state.mark_durable_edit();
    }
    if response.drag_stopped() || discrete {
        state.commit_timeline_history();
    }
}

fn title_color_controls(ui: &mut Ui, label: &str, color: &mut TitleColor) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut rgba = Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a);
        let response = ui.color_edit_button_srgba(&mut rgba);
        if response.changed() {
            let [r, g, b, a] = rgba.to_array();
            *color = TitleColor::rgba(r, g, b, a);
        }
        response
    })
    .inner
}

fn title_inspector(ui: &mut Ui, state: &mut EditorState) {
    let Some(title_id) = state.selected_title else {
        return;
    };
    let Some(original) = state.timeline.title(title_id).cloned() else {
        return;
    };
    egui::CollapsingHeader::new(t(state.language, "Title inspector", "タイトルインスペクタ"))
        .default_open(true)
        .show(ui, |ui| {
            ui.label(RichText::new(t(state.language, "Text", "テキスト")).strong());
            let draft = state
                .title_text_drafts
                .entry(title_id)
                .or_insert_with(|| original.text.clone());
            let text_response = ui.add(egui::TextEdit::multiline(draft).desired_rows(3));
            let text_commit = text_response.lost_focus()
                || (text_response.has_focus()
                    && ui.input(|input| {
                        input.key_pressed(egui::Key::Enter) && input.modifiers.ctrl
                    }));
            if text_commit && !draft.trim().is_empty() && *draft != original.text {
                let mut next = original.clone();
                next.text = draft.clone();
                let before = state.timeline_history_checkpoint();
                if state.timeline.replace_title(title_id, next).is_ok() {
                    state.record_timeline_history(before);
                    state.mark_durable_edit();
                }
            }
            if text_commit {
                state.title_text_drafts.remove(&title_id);
            }

            let mut title = state
                .timeline
                .title(title_id)
                .cloned()
                .unwrap_or(original.clone());
            let response = ui.add(egui::Slider::new(&mut title.font_size, 4.0..=512.0).text(t(
                state.language,
                "Size",
                "サイズ",
            )));
            title_live_edit(state, &response, title.clone());
            ui.horizontal(|ui| {
                ui.label(t(state.language, "Align", "揃え"));
                for (alignment, label) in [
                    (TitleAlignment::Left, "◀"),
                    (TitleAlignment::Center, "◆"),
                    (TitleAlignment::Right, "▶"),
                ] {
                    let mut response = ui.selectable_label(title.alignment == alignment, label);
                    if response.clicked() {
                        title.alignment = alignment;
                        response.mark_changed();
                        title_live_edit(state, &response, title.clone());
                    }
                }
            });
            let response =
                title_color_controls(ui, &t(state.language, "Fill", "塗り"), &mut title.fill);
            title_live_edit(state, &response, title.clone());
            let response = title_color_controls(
                ui,
                &t(state.language, "Outline", "アウトライン"),
                &mut title.outline_color,
            );
            title_live_edit(state, &response, title.clone());
            let response = ui.add(
                egui::Slider::new(&mut title.outline_width, 0.0..=32.0).text(t(
                    state.language,
                    "Outline width",
                    "アウトライン幅",
                )),
            );
            title_live_edit(state, &response, title.clone());
            let response = title_color_controls(
                ui,
                &t(state.language, "Shadow", "影"),
                &mut title.shadow_color,
            );
            title_live_edit(state, &response, title.clone());
            ui.horizontal(|ui| {
                let response =
                    ui.add(egui::Slider::new(&mut title.shadow_offset_x, -64.0..=64.0).text("X"));
                title_live_edit(state, &response, title.clone());
                let response =
                    ui.add(egui::Slider::new(&mut title.shadow_offset_y, -64.0..=64.0).text("Y"));
                title_live_edit(state, &response, title.clone());
                let response = ui.add(
                    egui::Slider::new(&mut title.shadow_blur, 0.0..=32.0).text(t(
                        state.language,
                        "Blur",
                        "ぼかし",
                    )),
                );
                title_live_edit(state, &response, title.clone());
            });
            ui.horizontal(|ui| {
                let response =
                    ui.add(egui::Slider::new(&mut title.position_x, 0.0..=1.0).text("X"));
                title_live_edit(state, &response, title.clone());
                let response =
                    ui.add(egui::Slider::new(&mut title.position_y, 0.0..=1.0).text("Y"));
                title_live_edit(state, &response, title.clone());
            });
            let response = ui.add(egui::Slider::new(&mut title.opacity, 0.0..=1.0).text(t(
                state.language,
                "Opacity",
                "不透明度",
            )));
            title_live_edit(state, &response, title.clone());
            let response = ui.add(
                egui::Slider::new(&mut title.fade_in.0, 0..=title.duration.0).text(t(
                    state.language,
                    "Fade in",
                    "フェードイン",
                )),
            );
            title_live_edit(state, &response, title.clone());
            let response = ui.add(
                egui::Slider::new(&mut title.fade_out.0, 0..=title.duration.0).text(t(
                    state.language,
                    "Fade out",
                    "フェードアウト",
                )),
            );
            title_live_edit(state, &response, title.clone());
            let response = ui.checkbox(&mut title.enabled, t(state.language, "Visible", "表示"));
            title_live_edit(state, &response, title.clone());
            if ui
                .button(t(state.language, "Delete title", "タイトルを削除"))
                .clicked()
            {
                state.delete_selected_timeline_clip();
            }
        });
}

fn audio_crossfade_inspector(ui: &mut Ui, state: &mut EditorState) {
    let Some(clip_id) = state.selected_timeline_clip else {
        return;
    };
    let Some(clip) = state.timeline.clip(clip_id).cloned() else {
        return;
    };
    let Some(track) = state.timeline.track(clip.track_id).cloned() else {
        return;
    };
    if track.kind != TrackKind::Audio {
        return;
    }
    let index = track
        .clips
        .iter()
        .position(|candidate| candidate.id == clip_id)
        .unwrap_or(0);
    let left = index
        .checked_sub(1)
        .and_then(|index| track.clips.get(index))
        .cloned();
    let right = track.clips.get(index + 1).cloned();
    egui::CollapsingHeader::new(t(state.language, "Crossfade", "クロスフェード"))
        .default_open(true)
        .show(ui, |ui| {
            for (left_clip, right_clip, edge_label) in [
                (
                    left.as_ref(),
                    Some(&clip),
                    t(state.language, "At start", "先頭"),
                ),
                (
                    Some(&clip),
                    right.as_ref(),
                    t(state.language, "At end", "終端"),
                ),
            ] {
                let (Some(left_clip), Some(right_clip)) = (left_clip, right_clip) else {
                    continue;
                };
                if left_clip.end() != right_clip.start {
                    continue;
                }
                audio_crossfade_edge_inspector(
                    ui,
                    state,
                    track.id,
                    left_clip,
                    right_clip,
                    &edge_label,
                );
            }
        });
}

fn audio_crossfade_edge_inspector(
    ui: &mut Ui,
    state: &mut EditorState,
    track_id: TrackId,
    left_clip: &Clip,
    right_clip: &Clip,
    edge_label: &str,
) {
    let transition = state
        .timeline
        .audio_transitions()
        .iter()
        .find(|transition| {
            transition.left_clip == left_clip.id && transition.right_clip == right_clip.id
        })
        .cloned();
    ui.group(|ui| {
        ui.label(RichText::new(edge_label).strong());
        if let Some(transition) = transition {
            let mut duration = transition.duration.0;
            let capacity = state
                .audio_transition_duration_capacity(
                    transition.left_clip,
                    transition.right_clip,
                    Some(transition.id),
                )
                .map_or(duration, |capacity| capacity.0.max(duration))
                .max(1);
            let response = ui.add(egui::Slider::new(&mut duration, 1..=capacity).text(t(
                state.language,
                "Equal-power duration",
                "イコールパワー時間",
            )));
            if response.drag_started() || (response.changed() && !response.dragged()) {
                state.begin_timeline_history();
            }
            if response.changed()
                && state
                    .timeline
                    .replace_audio_transition(transition.id, Tick(duration))
                    .is_ok()
            {
                state.mark_durable_edit();
            }
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                state.commit_timeline_history();
            }
            if ui
                .button(t(
                    state.language,
                    "Remove crossfade",
                    "クロスフェードを削除",
                ))
                .clicked()
            {
                let before = state.timeline_history_checkpoint();
                if state
                    .timeline
                    .remove_audio_transition(transition.id)
                    .is_ok()
                {
                    state.record_timeline_history(before);
                    state.mark_durable_edit();
                }
            }
        } else {
            let capacity =
                state.audio_transition_duration_capacity(left_clip.id, right_clip.id, None);
            if ui
                .add_enabled(
                    capacity.is_some_and(|capacity| capacity.0 > 0),
                    egui::Button::new(t(
                        state.language,
                        "Add equal-power crossfade",
                        "イコールパワークロスフェードを追加",
                    )),
                )
                .on_hover_text(t(
                    state.language,
                    "Requires unused source audio before and after the cut",
                    "カット前後に未使用の音声素材が必要です",
                ))
                .clicked()
            {
                let before = state.timeline_history_checkpoint();
                let duration = Tick(
                    DEFAULT_VIDEO_TRANSITION_DURATION
                        .0
                        .min(capacity.map_or(0, |capacity| capacity.0)),
                );
                if duration.0 > 0
                    && state
                        .timeline
                        .add_audio_transition(track_id, left_clip.id, right_clip.id, duration)
                        .is_ok()
                {
                    state.record_timeline_history(before);
                    state.mark_durable_edit();
                }
            }
        }
    });
}

#[derive(Clone, Copy)]
enum InspectorScrubUnit {
    Percent,
    Degrees,
    Raw,
    Stops,
}

/// A compact Premiere-style value: drag horizontally to scrub, or click to type.
/// The stored value remains in timeline units; only the presentation is scaled.
fn inspector_scrub_value(
    ui: &mut Ui,
    label: String,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    unit: InspectorScrubUnit,
) -> egui::Response {
    let mut response = None;
    ui.horizontal(|ui| {
        ui.add_sized(
            [94.0, 20.0],
            egui::Label::new(RichText::new(label).size(11.0)),
        );
        response = Some(inspector_scrub_numeric_value(ui, value, range, unit));
    });
    response.expect("the compact inspector value row always adds its DragValue")
}

/// Numeric half of [`inspector_scrub_value`], shared by the compact color rows.
fn inspector_scrub_numeric_value(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    unit: InspectorScrubUnit,
) -> egui::Response {
    let (display_scale, suffix, stored_decimals, shown_decimals, speed) = match unit {
        // Keep sub-percent precision in timeline units even though the compact label only
        // needs one decimal place.
        InspectorScrubUnit::Percent => (100.0_f64, " %", 3, 1, 0.005),
        InspectorScrubUnit::Degrees => (1.0, "°", 1, 1, 0.25),
        InspectorScrubUnit::Raw => (1.0, "", 2, 2, 0.05),
        InspectorScrubUnit::Stops => (1.0, " st", 2, 2, 0.05),
    };
    let mut response = None;
    ui.scope(|ui| {
        ui.spacing_mut().interact_size = Vec2::new(86.0, 20.0);
        ui.style_mut().drag_value_text_style = egui::TextStyle::Small;
        ui.style_mut()
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(11.0));
        let scrub_blue = Color32::from_rgb(74, 157, 255);
        let scrub_hover = Color32::from_rgb(31, 49, 68);
        ui.visuals_mut().widgets.inactive.fg_stroke.color = scrub_blue;
        ui.visuals_mut().widgets.inactive.bg_fill = Color32::TRANSPARENT;
        ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        ui.visuals_mut().widgets.hovered.fg_stroke.color = scrub_blue;
        ui.visuals_mut().widgets.hovered.bg_fill = scrub_hover;
        ui.visuals_mut().widgets.hovered.weak_bg_fill = scrub_hover;
        ui.visuals_mut().widgets.active.fg_stroke.color = Color32::WHITE;
        ui.visuals_mut().widgets.active.bg_fill = Color32::from_rgb(38, 70, 100);
        ui.visuals_mut().widgets.active.weak_bg_fill = Color32::from_rgb(38, 70, 100);
        ui.scope(|ui| {
            // DragValue's atom text does not inherit the button foreground stroke, so use
            // a nested override that colors only the numeric field—not its row label.
            ui.visuals_mut().override_text_color = Some(scrub_blue);
            response = Some(
                ui.add(
                    egui::DragValue::new(value)
                        .range(range)
                        .speed(speed)
                        .fixed_decimals(stored_decimals)
                        // Text entry commits on Enter/focus loss, so typing one number produces
                        // one undo step. Pointer scrubbing still updates every drag frame.
                        .update_while_editing(false)
                        .custom_formatter(move |number, _| {
                            format!("{:.shown_decimals$}", number * display_scale)
                        })
                        .custom_parser(move |text| {
                            text.trim()
                                .trim_end_matches(|character: char| {
                                    matches!(character, '%' | '°' | 's' | 't')
                                })
                                .trim()
                                .parse::<f64>()
                                .ok()
                                .map(|number| number / display_scale)
                        })
                        .suffix(suffix),
                ),
            );
        });
    });
    response.expect("the compact inspector value row always adds its DragValue")
}

fn inspector_section(ui: &mut Ui, label: String) {
    ui.label(RichText::new(label).size(12.0).strong());
}

fn clip_inspector(ui: &mut Ui, state: &mut EditorState) {
    let Some(clip_id) = state.selected_timeline_clip else {
        return;
    };
    let Some(clip) = state.timeline.clip(clip_id).cloned() else {
        return;
    };
    let kind = state.timeline.track(clip.track_id).map(|track| track.kind);
    egui::CollapsingHeader::new(
        RichText::new(t(state.language, "Inspector", "インスペクタ")).size(12.0),
    )
    .default_open(true)
    .show(ui, |ui| {
        let mut enabled = clip.enabled;
        if ui
            .checkbox(&mut enabled, t(state.language, "Enabled", "有効"))
            .on_hover_text(t(
                state.language,
                "Disabled clips remain editable but do not play or export.",
                "無効なクリップは編集可能なままですが、再生や書き出しには含まれません。",
            ))
            .changed()
        {
            state.set_timeline_clip_enabled(clip_id, enabled);
        }
        ui.separator();
        if kind == Some(TrackKind::Video) {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(t(state.language, "Transform", "トランスフォーム"))
                        .size(12.0)
                        .strong(),
                );
                if ui
                    .small_button(t(state.language, "Reset Transform", "変形をリセット"))
                    .on_hover_text(t(
                        state.language,
                        "Restore position, scale, crop, rotation, anchor, and flip.",
                        "位置、スケール、クロップ、回転、アンカー、反転を初期化します。",
                    ))
                    .clicked()
                {
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_clip_transform(clip_id, nle_timeline::ClipTransform::default())
                    });
                }
            });
            let mut xf = clip.transform;
            ui.horizontal(|ui| {
                ui.label(RichText::new(t(state.language, "Sizing", "サイズ調整")).size(11.0));
                egui::ComboBox::from_id_salt(("clip-sizing", clip_id))
                    .selected_text(
                        RichText::new(sizing_mode_label(state.language, xf.sizing_mode)).size(11.0),
                    )
                    .show_ui(ui, |ui| {
                        for mode in [
                            nle_timeline::ClipSizingMode::Fit,
                            nle_timeline::ClipSizingMode::Fill,
                            nle_timeline::ClipSizingMode::Stretch,
                            nle_timeline::ClipSizingMode::Original,
                        ] {
                            ui.selectable_value(
                                &mut xf.sizing_mode,
                                mode,
                                sizing_mode_label(state.language, mode),
                            )
                            .on_hover_text(sizing_mode_tooltip(state.language, mode));
                        }
                    });
            });
            if xf.sizing_mode != clip.transform.sizing_mode {
                apply_track_header_edit(state, |timeline| timeline.set_clip_transform(clip_id, xf));
            }
            let opacity_response = inspector_scrub_value(
                ui,
                t(state.language, "Opacity", "不透明度"),
                &mut xf.opacity,
                nle_timeline::ClipTransform::MIN_OPACITY..=nle_timeline::ClipTransform::MAX_OPACITY,
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &opacity_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            ui.separator();
            inspector_section(ui, t(state.language, "Position & Scale", "位置とスケール"));
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut state.inspector_scale_linked,
                    t(state.language, "Link scale", "スケールを連動"),
                );
            });
            let scale_x_response = inspector_scrub_value(
                ui,
                t(state.language, "Scale X", "スケール X"),
                &mut xf.scale_x,
                nle_timeline::ClipTransform::MIN_SCALE..=nle_timeline::ClipTransform::MAX_SCALE,
                InspectorScrubUnit::Percent,
            );
            if state.inspector_scale_linked && scale_x_response.changed() {
                xf.scale_y = xf.scale_x;
            }
            mixer_live_edit(state, &scale_x_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let scale_y_response = inspector_scrub_value(
                ui,
                t(state.language, "Scale Y", "スケール Y"),
                &mut xf.scale_y,
                nle_timeline::ClipTransform::MIN_SCALE..=nle_timeline::ClipTransform::MAX_SCALE,
                InspectorScrubUnit::Percent,
            );
            if state.inspector_scale_linked && scale_y_response.changed() {
                xf.scale_x = xf.scale_y;
            }
            mixer_live_edit(state, &scale_y_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let pos_x_response = inspector_scrub_value(
                ui,
                t(state.language, "Position X", "位置 X"),
                &mut xf.pos_x,
                nle_timeline::ClipTransform::MIN_POS..=nle_timeline::ClipTransform::MAX_POS,
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &pos_x_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let pos_y_response = inspector_scrub_value(
                ui,
                t(state.language, "Position Y", "位置 Y"),
                &mut xf.pos_y,
                nle_timeline::ClipTransform::MIN_POS..=nle_timeline::ClipTransform::MAX_POS,
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &pos_y_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            ui.separator();
            inspector_section(ui, t(state.language, "Rotation & Anchor", "回転とアンカー"));
            let rotation_response = inspector_scrub_value(
                ui,
                t(state.language, "Rotation", "回転"),
                &mut xf.rotation_degrees,
                nle_timeline::ClipTransform::MIN_ROTATION_DEGREES
                    ..=nle_timeline::ClipTransform::MAX_ROTATION_DEGREES,
                InspectorScrubUnit::Degrees,
            );
            mixer_live_edit(state, &rotation_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let anchor_x_response = inspector_scrub_value(
                ui,
                t(state.language, "Anchor X", "アンカー X"),
                &mut xf.anchor_x,
                nle_timeline::ClipTransform::MIN_ANCHOR..=nle_timeline::ClipTransform::MAX_ANCHOR,
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &anchor_x_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let anchor_y_response = inspector_scrub_value(
                ui,
                t(state.language, "Anchor Y", "アンカー Y"),
                &mut xf.anchor_y,
                nle_timeline::ClipTransform::MIN_ANCHOR..=nle_timeline::ClipTransform::MAX_ANCHOR,
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &anchor_y_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            ui.separator();
            inspector_section(ui, t(state.language, "Crop", "クロップ"));
            let crop_left_response = inspector_scrub_value(
                ui,
                t(state.language, "Left", "左"),
                &mut xf.crop_left,
                0.0..=crop_edge_max(xf.crop_right),
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &crop_left_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let crop_right_response = inspector_scrub_value(
                ui,
                t(state.language, "Right", "右"),
                &mut xf.crop_right,
                0.0..=crop_edge_max(xf.crop_left),
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &crop_right_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let crop_top_response = inspector_scrub_value(
                ui,
                t(state.language, "Top", "上"),
                &mut xf.crop_top,
                0.0..=crop_edge_max(xf.crop_bottom),
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &crop_top_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            let crop_bottom_response = inspector_scrub_value(
                ui,
                t(state.language, "Bottom", "下"),
                &mut xf.crop_bottom,
                0.0..=crop_edge_max(xf.crop_top),
                InspectorScrubUnit::Percent,
            );
            mixer_live_edit(state, &crop_bottom_response, |timeline| {
                timeline.set_clip_transform(clip_id, xf)
            });
            ui.separator();
            inspector_section(ui, t(state.language, "Flip", "反転"));
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(xf.flip_h, t(state.language, "Flip H", "左右反転"))
                    .clicked()
                {
                    xf.flip_h = !xf.flip_h;
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_clip_transform(clip_id, xf)
                    });
                }
                if ui
                    .selectable_label(xf.flip_v, t(state.language, "Flip V", "上下反転"))
                    .clicked()
                {
                    xf.flip_v = !xf.flip_v;
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_clip_transform(clip_id, xf)
                    });
                }
            });
            ui.separator();
            video_transition_inspector(
                ui,
                state,
                &clip,
                &t(state.language, "Video Transitions", "ビデオトランジション"),
            );
        }
    });
}

fn audio_clip_controls(ui: &mut Ui, state: &mut EditorState, clip: &Clip) {
    ui.label(RichText::new(t(state.language, "Clip controls", "クリップコントロール")).strong());
    let mut gain = clip.gain_db;
    let gain_response = ui.add(
        egui::Slider::new(&mut gain, MIN_GAIN_DB..=MAX_GAIN_DB)
            .text(t(state.language, "Volume", "音量"))
            .suffix(" dB"),
    );
    mixer_live_edit(state, &gain_response, |timeline| {
        timeline.set_audio_gain(clip.id, gain)
    });
    let mut left = clip.gain_left_db;
    let mut right = clip.gain_right_db;
    let left_response = ui.add(
        egui::Slider::new(&mut left, MIN_GAIN_DB..=MAX_GAIN_DB)
            .text(t(state.language, "Left", "左"))
            .suffix(" dB"),
    );
    mixer_live_edit(state, &left_response, |timeline| {
        timeline.set_audio_channel_gain(clip.id, left, right)
    });
    let right_response = ui.add(
        egui::Slider::new(&mut right, MIN_GAIN_DB..=MAX_GAIN_DB)
            .text(t(state.language, "Right", "右"))
            .suffix(" dB"),
    );
    mixer_live_edit(state, &right_response, |timeline| {
        timeline.set_audio_channel_gain(clip.id, left, right)
    });
    ui.add_space(4.0);
    ui.separator();
    audio_effects_rack(ui, state, AudioEffectsScope::Clip(clip.id));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioEffectsScope {
    Clip(ClipId),
    Track(TrackId),
}

impl AudioEffectsScope {
    fn effects(self, timeline: &Timeline) -> Option<&[AudioEffect]> {
        match self {
            Self::Clip(clip_id) => timeline.clip(clip_id).map(|clip| clip.effects.as_slice()),
            Self::Track(track_id) => timeline
                .track(track_id)
                .map(|track| track.effects.as_slice()),
        }
    }

    fn set_effects(
        self,
        timeline: &mut Timeline,
        effects: Vec<AudioEffect>,
    ) -> Result<(), TimelineError> {
        match self {
            Self::Clip(clip_id) => timeline.set_clip_audio_effects(clip_id, effects),
            Self::Track(track_id) => timeline.set_track_audio_effects(track_id, effects),
        }
    }

    fn rack_label(self, language: Language) -> String {
        match self {
            Self::Clip(_) => t(language, "Clip Effects", "クリップエフェクト"),
            Self::Track(_) => t(language, "Track Effects", "トラックエフェクト"),
        }
    }
}

fn audio_effect_base(effect: &AudioEffect) -> &AudioEffect {
    match effect {
        AudioEffect::Bypassed(effect) => effect,
        effect => effect,
    }
}

fn audio_effect_enabled(effect: &AudioEffect) -> bool {
    !matches!(effect, AudioEffect::Bypassed(_))
}

fn audio_effect_with_enabled(effect: &AudioEffect, enabled: bool) -> AudioEffect {
    match (enabled, effect) {
        (true, AudioEffect::Bypassed(effect)) => (**effect).clone(),
        (false, AudioEffect::Bypassed(_)) => effect.clone(),
        (false, effect) => AudioEffect::Bypassed(Box::new(effect.clone())),
        (true, effect) => effect.clone(),
    }
}

fn audio_effect_is_realtime_supported(effect: &AudioEffect) -> bool {
    matches!(
        audio_effect_base(effect),
        AudioEffect::HighPass { .. }
            | AudioEffect::LowPass { .. }
            | AudioEffect::Eq { .. }
            | AudioEffect::StereoWidth { .. }
    )
}

fn audio_effect_blocks_export(effect: &AudioEffect) -> bool {
    audio_effect_enabled(effect) && !audio_effect_is_realtime_supported(effect)
}

fn audio_effect_label(language: Language, effect: &AudioEffect) -> String {
    match audio_effect_base(effect) {
        AudioEffect::Normalize => t(language, "Normalize", "ノーマライズ"),
        AudioEffect::Chorus => t(language, "Chorus", "コーラス"),
        AudioEffect::DeEsser => t(language, "De-Esser", "ディエッサー"),
        AudioEffect::DeHummer => t(language, "De-Hummer", "ハム除去"),
        AudioEffect::Delay => t(language, "Delay", "ディレイ"),
        AudioEffect::DialogueProcessor => {
            t(language, "Dialogue Processor", "ダイアログプロセッサー")
        }
        AudioEffect::Distortion => t(language, "Distortion", "ディストーション"),
        AudioEffect::LowPass { .. } => t(language, "Low-Pass Filter", "ローパスフィルター"),
        AudioEffect::Lfe { .. } => t(language, "LFE Filter", "LFEフィルター"),
        AudioEffect::HighPass { .. } => t(language, "High-Pass Filter", "ハイパスフィルター"),
        AudioEffect::Eq { .. } => t(language, "Parametric EQ", "パラメトリックEQ"),
        AudioEffect::Compressor => t(language, "Compressor", "コンプレッサー"),
        AudioEffect::Limiter => t(language, "Limiter", "リミッター"),
        AudioEffect::Modulation => t(language, "Modulation", "モジュレーション"),
        AudioEffect::MultibandCompressor => t(
            language,
            "Multiband Compressor",
            "マルチバンドコンプレッサー",
        ),
        AudioEffect::NoiseReduction => t(language, "Noise Reduction", "ノイズリダクション"),
        AudioEffect::Pitch { .. } => t(language, "Pitch", "ピッチ"),
        AudioEffect::Echo => t(language, "Echo", "エコー"),
        AudioEffect::Reverb => t(language, "Reverb", "リバーブ"),
        AudioEffect::Flanger => t(language, "Flanger", "フランジャー"),
        AudioEffect::SoftClipper => t(language, "Soft Clipper", "ソフトクリッパー"),
        AudioEffect::StereoFixer => t(language, "Stereo Fixer", "ステレオ修正"),
        AudioEffect::StereoWidth { .. } => t(language, "Stereo Width", "ステレオ幅"),
        AudioEffect::Tremolo => t(language, "Tremolo", "トレモロ"),
        AudioEffect::VocalChannel => t(language, "Vocal Channel", "ボーカルチャンネル"),
        AudioEffect::Bypassed(_) => unreachable!("audio_effect_base unwraps bypass"),
    }
}

fn audio_effect_description(language: Language, effect: &AudioEffect) -> String {
    match audio_effect_base(effect) {
        AudioEffect::HighPass { .. } => t(
            language,
            "Remove rumble and low-frequency noise below the cutoff.",
            "カットオフ以下の低域ノイズや振動を除去します。",
        ),
        AudioEffect::LowPass { .. } => t(
            language,
            "Soften hiss and frequencies above the cutoff.",
            "カットオフ以上のヒスや高域を抑えます。",
        ),
        AudioEffect::Eq { .. } => t(
            language,
            "Boost or cut a focused frequency band.",
            "指定した周波数帯域をブーストまたはカットします。",
        ),
        AudioEffect::StereoWidth { .. } => t(
            language,
            "Narrow or widen the stereo side signal.",
            "ステレオのサイド信号を狭めたり広げたりします。",
        ),
        _ => t(
            language,
            "Stored project effect. Live preview and export are not available yet.",
            "プロジェクトに保存されたエフェクトです。ライブプレビューと書き出しは未対応です。",
        ),
    }
}

fn add_audio_effect(state: &mut EditorState, scope: AudioEffectsScope, effect: AudioEffect) {
    let Some(mut effects) = scope.effects(&state.timeline).map(ToOwned::to_owned) else {
        return;
    };
    if effects.len() >= MAX_AUDIO_EFFECTS_PER_SCOPE {
        return;
    }
    effects.push(effect);
    apply_track_header_edit(state, |timeline| scope.set_effects(timeline, effects));
}

fn audio_effect_add_menu(ui: &mut Ui, state: &mut EditorState, scope: AudioEffectsScope) {
    ui.menu_button(
        t(state.language, "+ Add Effect", "+ エフェクトを追加"),
        |ui| {
            ui.set_min_width(190.0);
            ui.menu_button(t(state.language, "Filters", "フィルター"), |ui| {
                for (label, effect) in [
                    (
                        t(state.language, "High-Pass Filter", "ハイパスフィルター"),
                        AudioEffect::HighPass { hz: 80 },
                    ),
                    (
                        t(state.language, "Low-Pass Filter", "ローパスフィルター"),
                        AudioEffect::LowPass { hz: 18_000 },
                    ),
                ] {
                    if ui.button(label).clicked() {
                        add_audio_effect(state, scope, effect);
                        ui.close();
                    }
                }
            });
            ui.menu_button(
                t(state.language, "Equalization", "イコライザー"),
                |ui| {
                    if ui
                        .button(t(state.language, "Parametric EQ", "パラメトリックEQ"))
                        .clicked()
                    {
                        add_audio_effect(state, scope, AudioEffect::Eq { hz: 1_000, db: 0.0 });
                        ui.close();
                    }
                },
            );
            ui.menu_button(t(state.language, "Stereo", "ステレオ"), |ui| {
                if ui
                    .button(t(state.language, "Stereo Width", "ステレオ幅"))
                    .clicked()
                {
                    add_audio_effect(state, scope, AudioEffect::StereoWidth { width: 1.0 });
                    ui.close();
                }
            });
        },
    );
}

fn audio_effects_rack(ui: &mut Ui, state: &mut EditorState, scope: AudioEffectsScope) {
    let Some(effects) = scope.effects(&state.timeline).map(ToOwned::to_owned) else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(scope.rack_label(state.language)).strong());
        ui.label(
            RichText::new(format!("{} / {MAX_AUDIO_EFFECTS_PER_SCOPE}", effects.len()))
                .small()
                .color(Color32::from_rgb(111, 185, 211)),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_enabled_ui(effects.len() < MAX_AUDIO_EFFECTS_PER_SCOPE, |ui| {
                audio_effect_add_menu(ui, state, scope);
            });
        });
    });
    ui.label(
        RichText::new(t(
            state.language,
            "Processed from top to bottom · drag controls for live preview",
            "上から下へ処理 · コントロールをドラッグしてライブプレビュー",
        ))
        .small()
        .color(Color32::from_rgb(132, 151, 165)),
    );
    if effects.is_empty() {
        Frame::new()
            .fill(Color32::from_rgb(16, 23, 29))
            .stroke(Stroke::new(1.0, Color32::from_rgb(38, 53, 63)))
            .inner_margin(egui::Margin::symmetric(8, 7))
            .show(ui, |ui| {
                ui.small(t(
                    state.language,
                    "Add a filter, EQ band, or stereo processor.",
                    "フィルター、EQバンド、ステレオ処理を追加します。",
                ));
            });
        return;
    }
    for (index, effect) in effects.iter().enumerate() {
        audio_effect_card(ui, state, scope, &effects, index, effect);
        ui.add_space(4.0);
    }
}

fn audio_effect_card_actions(
    ui: &mut Ui,
    state: &mut EditorState,
    scope: AudioEffectsScope,
    effects: &[AudioEffect],
    index: usize,
) {
    ui.menu_button("⋯", |ui| {
        if ui
            .add_enabled(
                index > 0,
                egui::Button::new(t(state.language, "Move earlier", "前へ移動")),
            )
            .clicked()
        {
            let mut effects = effects.to_vec();
            effects.swap(index, index - 1);
            apply_track_header_edit(state, |timeline| scope.set_effects(timeline, effects));
            ui.close();
        }
        if ui
            .add_enabled(
                index + 1 < effects.len(),
                egui::Button::new(t(state.language, "Move later", "後へ移動")),
            )
            .clicked()
        {
            let mut effects = effects.to_vec();
            effects.swap(index, index + 1);
            apply_track_header_edit(state, |timeline| scope.set_effects(timeline, effects));
            ui.close();
        }
        ui.separator();
        if ui.button(t(state.language, "Remove", "削除")).clicked() {
            let mut effects = effects.to_vec();
            effects.remove(index);
            apply_track_header_edit(state, |timeline| scope.set_effects(timeline, effects));
            ui.close();
        }
    });
}

fn live_replace_audio_effect(
    state: &mut EditorState,
    response: &egui::Response,
    scope: AudioEffectsScope,
    effects: &[AudioEffect],
    index: usize,
    replacement: AudioEffect,
    enabled: bool,
) {
    let mut effects = effects.to_vec();
    effects[index] = audio_effect_with_enabled(&replacement, enabled);
    mixer_live_edit(state, response, |timeline| {
        scope.set_effects(timeline, effects)
    });
}

fn audio_effect_card(
    ui: &mut Ui,
    state: &mut EditorState,
    scope: AudioEffectsScope,
    effects: &[AudioEffect],
    index: usize,
    effect: &AudioEffect,
) {
    let enabled = audio_effect_enabled(effect);
    let supported = audio_effect_is_realtime_supported(effect);
    let label = audio_effect_label(state.language, effect);
    Frame::new()
        .fill(if enabled {
            Color32::from_rgb(18, 28, 36)
        } else {
            Color32::from_rgb(16, 21, 27)
        })
        .stroke(Stroke::new(
            1.0,
            if supported || !enabled {
                Color32::from_rgb(47, 72, 85)
            } else {
                Color32::from_rgb(137, 86, 65)
            },
        ))
        .inner_margin(egui::Margin::symmetric(8, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut next_enabled = enabled;
                if ui
                    .checkbox(&mut next_enabled, "")
                    .on_hover_text(t(
                        state.language,
                        "Bypass without losing settings",
                        "設定を保持したままバイパス",
                    ))
                    .changed()
                {
                    let mut effects = effects.to_vec();
                    effects[index] = audio_effect_with_enabled(effect, next_enabled);
                    apply_track_header_edit(state, |timeline| scope.set_effects(timeline, effects));
                }
                ui.label(RichText::new(format!("{}  {label}", index + 1)).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    audio_effect_card_actions(ui, state, scope, effects, index);
                });
            });
            ui.label(
                RichText::new(audio_effect_description(state.language, effect))
                    .small()
                    .color(if supported {
                        Color32::from_rgb(137, 158, 171)
                    } else {
                        Color32::from_rgb(217, 148, 108)
                    }),
            );
            if !supported {
                return;
            }
            ui.add_space(2.0);
            match audio_effect_base(effect) {
                AudioEffect::HighPass { hz } => {
                    let mut value = *hz;
                    let response = ui.add(
                        egui::Slider::new(
                            &mut value,
                            AudioEffect::MIN_FILTER_HZ..=AudioEffect::MAX_RENDER_FILTER_HZ,
                        )
                        .logarithmic(true)
                        .text(t(state.language, "Cutoff", "カットオフ"))
                        .suffix(" Hz"),
                    );
                    live_replace_audio_effect(
                        state,
                        &response,
                        scope,
                        effects,
                        index,
                        AudioEffect::HighPass { hz: value },
                        enabled,
                    );
                }
                AudioEffect::LowPass { hz } => {
                    let mut value = *hz;
                    let response = ui.add(
                        egui::Slider::new(
                            &mut value,
                            AudioEffect::MIN_FILTER_HZ..=AudioEffect::MAX_RENDER_FILTER_HZ,
                        )
                        .logarithmic(true)
                        .text(t(state.language, "Cutoff", "カットオフ"))
                        .suffix(" Hz"),
                    );
                    live_replace_audio_effect(
                        state,
                        &response,
                        scope,
                        effects,
                        index,
                        AudioEffect::LowPass { hz: value },
                        enabled,
                    );
                }
                AudioEffect::Eq { hz, db } => {
                    let mut frequency = *hz;
                    let mut gain = *db;
                    let frequency_response = ui.add(
                        egui::Slider::new(
                            &mut frequency,
                            AudioEffect::MIN_FILTER_HZ..=AudioEffect::MAX_RENDER_FILTER_HZ,
                        )
                        .logarithmic(true)
                        .text(t(state.language, "Frequency", "周波数"))
                        .suffix(" Hz"),
                    );
                    live_replace_audio_effect(
                        state,
                        &frequency_response,
                        scope,
                        effects,
                        index,
                        AudioEffect::Eq {
                            hz: frequency,
                            db: gain,
                        },
                        enabled,
                    );
                    let gain_response = ui.add(
                        egui::Slider::new(
                            &mut gain,
                            AudioEffect::MIN_EQ_DB..=AudioEffect::MAX_EQ_DB,
                        )
                        .text(t(state.language, "Gain", "ゲイン"))
                        .suffix(" dB"),
                    );
                    live_replace_audio_effect(
                        state,
                        &gain_response,
                        scope,
                        effects,
                        index,
                        AudioEffect::Eq {
                            hz: frequency,
                            db: gain,
                        },
                        enabled,
                    );
                }
                AudioEffect::StereoWidth { width } => {
                    let mut percent = *width * 100.0;
                    let response = ui.add(
                        egui::Slider::new(&mut percent, 0.0..=200.0)
                            .text(t(state.language, "Width", "幅"))
                            .suffix(" %"),
                    );
                    live_replace_audio_effect(
                        state,
                        &response,
                        scope,
                        effects,
                        index,
                        AudioEffect::StereoWidth {
                            width: percent / 100.0,
                        },
                        enabled,
                    );
                }
                _ => {}
            }
        });
}

fn video_transition_inspector(ui: &mut Ui, state: &mut EditorState, clip: &Clip, heading: &str) {
    ui.label(RichText::new(heading).strong());
    let transitions = state
        .timeline
        .transitions()
        .iter()
        .filter(|transition| transition.left_clip == clip.id || transition.right_clip == clip.id)
        .cloned()
        .collect::<Vec<_>>();
    if transitions.is_empty() {
        ui.small(t(
            state.language,
            "No transition is applied to the selected clip.",
            "選択クリップにはトランジションが適用されていません。",
        ));
        return;
    }
    for transition in transitions {
        let edge = if transition.right_clip == clip.id {
            FadeEdge::In
        } else {
            FadeEdge::Out
        };
        let edge_label = match edge {
            FadeEdge::In => t(state.language, "Start", "先頭"),
            FadeEdge::Out => t(state.language, "End", "末尾"),
        };
        Frame::new()
            .fill(Color32::from_rgb(19, 31, 42))
            .stroke(Stroke::new(1.0, Color32::from_rgb(53, 111, 143)))
            .inner_margin(egui::Margin::symmetric(8, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} · {edge_label}",
                        video_transition_kind_label(state.language, transition.kind)
                    ));
                    if ui
                        .small_button(t(state.language, "Remove", "削除"))
                        .clicked()
                    {
                        apply_track_header_edit(state, |timeline| {
                            timeline.remove_video_transition(transition.id).map(|_| ())
                        });
                    }
                });
                let mut kind = transition.kind;
                egui::ComboBox::from_label(t(state.language, "Kind", "種類"))
                    .selected_text(video_transition_kind_label(state.language, kind))
                    .show_ui(ui, |ui| {
                        for candidate in VIDEO_TRANSITION_KINDS {
                            ui.selectable_value(
                                &mut kind,
                                candidate,
                                video_transition_kind_label(state.language, candidate),
                            );
                        }
                    });
                if kind != transition.kind
                    && state
                        .transition_duration_capacity(
                            transition.left_clip,
                            transition.right_clip,
                            kind,
                            Some(transition.id),
                        )
                        .is_some_and(|capacity| capacity.0 >= transition.duration.0)
                {
                    apply_track_header_edit(state, |timeline| {
                        let mut replacement = transition.clone();
                        replacement.kind = kind;
                        timeline.replace_video_transition(transition.id, replacement)
                    });
                }
                let minimum = state.frame_rate.frame_boundary_tick(1).0.max(1);
                let capacity = state
                    .transition_duration_capacity(
                        transition.left_clip,
                        transition.right_clip,
                        transition.kind,
                        Some(transition.id),
                    )
                    .unwrap_or(transition.duration)
                    .0
                    .max(transition.duration.0)
                    .max(minimum);
                let mut duration = transition.duration.0;
                let duration_response = ui.add(
                    egui::Slider::new(&mut duration, minimum..=capacity)
                        .text(t(state.language, "Duration", "長さ"))
                        .custom_formatter(|value, _| format!("{:.2} s", value / 1_000_000.0)),
                );
                mixer_live_edit(state, &duration_response, |timeline| {
                    let mut replacement = transition.clone();
                    replacement.duration = Tick(duration);
                    timeline.replace_video_transition(transition.id, replacement)
                });
                let mut curve = transition.curve;
                let curve_response = ui.add(egui::Slider::new(&mut curve, -1.0..=1.0).text(t(
                    state.language,
                    "Curve",
                    "カーブ",
                )));
                mixer_live_edit(state, &curve_response, |timeline| {
                    let mut replacement = transition.clone();
                    replacement.curve = curve;
                    timeline.replace_video_transition(transition.id, replacement)
                });
            });
    }
}

fn transition_catalog_item(
    ui: &mut Ui,
    state: &mut EditorState,
    kind: VideoTransitionKind,
    description: &'static str,
) {
    let label = video_transition_kind_label(state.language, kind);
    let response = ui
        .add(
            egui::Label::new(
                RichText::new(format!("::  {label}"))
                    .strong()
                    .color(Color32::from_rgb(205, 230, 241)),
            )
            .sense(Sense::click_and_drag()),
        )
        .on_hover_text(description)
        .on_hover_cursor(egui::CursorIcon::Grab);
    response.dnd_set_drag_payload(TransitionDragPayload { kind });
    if response.drag_started() || response.dragged() || response.is_pointer_button_down_on() {
        state.active_transition_drag = Some(kind);
    }
    response.context_menu(|ui| {
        ui.label(RichText::new(&label).strong());
        ui.separator();
        ui.menu_button(t(state.language, "Apply", "適用"), |ui| {
            for (edge, action) in [
                (
                    FadeEdge::In,
                    t(
                        state.language,
                        "To selected clip start",
                        "選択クリップの先頭に",
                    ),
                ),
                (
                    FadeEdge::Out,
                    t(
                        state.language,
                        "To selected clip end",
                        "選択クリップの末尾に",
                    ),
                ),
            ] {
                let selected = state.selected_timeline_clip;
                let available =
                    selected.is_some_and(|clip| state.can_add_video_transition(clip, edge, kind));
                if ui
                    .add_enabled(available, egui::Button::new(action))
                    .on_hover_text(if available {
                        "Apply at the selected clip's adjacent cut."
                    } else {
                        "Select a video clip beside an unused cut with enough source handles."
                    })
                    .clicked()
                {
                    let _ = state.add_video_transition(edge, kind);
                    ui.close();
                }
            }
        });
    });
}

fn transition_details(ui: &mut Ui, state: &mut EditorState) {
    egui::CollapsingHeader::new(
        RichText::new(t(state.language, "Video Transitions", "ビデオトランジション"))
            .strong(),
    )
    .default_open(true)
    .show(ui, |ui| {
        Frame::new()
            .fill(Color32::from_rgb(18, 29, 39))
            .stroke(Stroke::new(1.0, Color32::from_rgb(48, 82, 100)))
            .inner_margin(egui::Margin::symmetric(8, 7))
            .show(ui, |ui| {
                ui.label(RichText::new(t(state.language, "Drag to a cut", "カットへドラッグ"))
                    .small()
                    .strong()
                    .color(Color32::from_rgb(103, 206, 240)));
                ui.small(t(
                    state.language,
                    "Drop on the seam between two adjacent video clips. Right-click an item to apply it to the selected clip.",
                    "隣接するビデオクリップ間のカットにドロップします。右クリックで選択クリップに適用できます。",
                ));
                ui.add_space(4.0);
                transition_catalog_family(ui, state, "Dissolve", "ディゾルブ", &[
                    (VideoTransitionKind::CrossDissolve, "Blend both clips across the cut."),
                    (VideoTransitionKind::FilmDissolve, "A softly eased, film-style dissolve."),
                ]);
                transition_catalog_family(ui, state, "Fade", "フェード", &[
                    (VideoTransitionKind::DipToBlack, "Fade out to black, then fade in."),
                    (VideoTransitionKind::DipToWhite, "Fade out to white, then fade in."),
                ]);
                transition_catalog_family(ui, state, "Wipe", "ワイプ", &[
                    (VideoTransitionKind::WipeLeft, "Reveal the incoming clip from the left edge."),
                    (VideoTransitionKind::WipeRight, "Reveal the incoming clip from the right edge."),
                    (VideoTransitionKind::WipeUp, "Reveal the incoming clip from the top edge."),
                    (VideoTransitionKind::WipeDown, "Reveal the incoming clip from the bottom edge."),
                ]);
                transition_catalog_family(ui, state, "Slide", "スライド", &[
                    (VideoTransitionKind::SlideFromLeft, "Slide the incoming clip in from the left."),
                    (VideoTransitionKind::SlideFromRight, "Slide the incoming clip in from the right."),
                    (VideoTransitionKind::SlideFromTop, "Slide the incoming clip in from the top."),
                    (VideoTransitionKind::SlideFromBottom, "Slide the incoming clip in from the bottom."),
                ]);
            });
    });
    ui.separator();
    if let Some(clip) = state
        .selected_timeline_clip
        .and_then(|clip_id| state.timeline.clip(clip_id).cloned())
        .filter(|clip| {
            state
                .timeline
                .track(clip.track_id)
                .is_some_and(|track| track.kind == TrackKind::Video)
        })
    {
        video_transition_inspector(
            ui,
            state,
            &clip,
            &t(
                state.language,
                "Selected Clip Settings",
                "選択クリップの設定",
            ),
        );
    } else {
        ui.label(t(
            state.language,
            "Select a video clip to edit its applied transition settings.",
            "適用済みトランジションの設定を編集するにはビデオクリップを選択してください。",
        ));
    }
}

fn video_transition_kind_label(language: Language, kind: VideoTransitionKind) -> String {
    match kind {
        VideoTransitionKind::CrossDissolve => t(language, "Cross Dissolve", "クロスディゾルブ"),
        VideoTransitionKind::FilmDissolve => t(language, "Film Dissolve", "フィルムディゾルブ"),
        VideoTransitionKind::DipToBlack => t(language, "Dip to Black", "黒へディップ"),
        VideoTransitionKind::DipToWhite => t(language, "Dip to White", "白へディップ"),
        VideoTransitionKind::WipeLeft => t(language, "Wipe Left", "左ワイプ"),
        VideoTransitionKind::WipeRight => t(language, "Wipe Right", "右ワイプ"),
        VideoTransitionKind::WipeUp => t(language, "Wipe Up", "上ワイプ"),
        VideoTransitionKind::WipeDown => t(language, "Wipe Down", "下ワイプ"),
        VideoTransitionKind::SlideFromLeft => t(language, "Slide From Left", "左からスライド"),
        VideoTransitionKind::SlideFromRight => t(language, "Slide From Right", "右からスライド"),
        VideoTransitionKind::SlideFromTop => t(language, "Slide From Top", "上からスライド"),
        VideoTransitionKind::SlideFromBottom => t(language, "Slide From Bottom", "下からスライド"),
    }
}

fn shaped_transition_progress(duration: Tick, curve: f32, progress: f32) -> f32 {
    fade_envelope_value(Fade { duration, curve }, progress)
}

fn transition_catalog_family(
    ui: &mut Ui,
    state: &mut EditorState,
    english: &'static str,
    japanese: &'static str,
    items: &[(VideoTransitionKind, &'static str)],
) {
    egui::CollapsingHeader::new(t(state.language, english, japanese))
        .default_open(matches!(english, "Dissolve" | "Fade"))
        .show(ui, |ui| {
            for &(kind, description) in items {
                transition_catalog_item(ui, state, kind, description);
            }
        });
}

fn transition_timeline_colors(kind: VideoTransitionKind, selected: bool) -> (Color32, Color32) {
    let (fill, stroke) = match kind {
        VideoTransitionKind::CrossDissolve | VideoTransitionKind::FilmDissolve => (
            Color32::from_rgba_unmultiplied(47, 122, 162, 158),
            Color32::from_rgb(159, 222, 244),
        ),
        VideoTransitionKind::DipToBlack | VideoTransitionKind::DipToWhite => (
            Color32::from_rgba_unmultiplied(20, 27, 34, 210),
            Color32::from_rgb(224, 224, 224),
        ),
        VideoTransitionKind::WipeLeft
        | VideoTransitionKind::WipeRight
        | VideoTransitionKind::WipeUp
        | VideoTransitionKind::WipeDown => (
            Color32::from_rgba_unmultiplied(37, 91, 72, 188),
            Color32::from_rgb(151, 226, 181),
        ),
        VideoTransitionKind::SlideFromLeft
        | VideoTransitionKind::SlideFromRight
        | VideoTransitionKind::SlideFromTop
        | VideoTransitionKind::SlideFromBottom => (
            Color32::from_rgba_unmultiplied(91, 60, 137, 188),
            Color32::from_rgb(206, 175, 248),
        ),
    };
    if selected {
        (fill.gamma_multiply(1.35), stroke.gamma_multiply(1.15))
    } else {
        (fill, stroke)
    }
}

fn inspector_source_tick(state: &EditorState, clip: &Clip) -> Tick {
    let relative = state
        .playhead
        .0
        .saturating_sub(clip.start.0)
        .clamp(0, clip.duration.0);
    Tick(clip.source_in.0.saturating_add(relative))
}

fn color_correction_inspector(ui: &mut Ui, state: &mut EditorState, clip: &Clip) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(t(state.language, "Color Effects", "カラーエフェクト")).strong());
        ui.label(
            RichText::new(format!(
                "{} / {MAX_VIDEO_EFFECTS_PER_CLIP}",
                clip.video_effects.len()
            ))
            .small()
            .color(Color32::from_rgb(111, 185, 211)),
        );
        let can_add = clip.video_effects.len() < MAX_VIDEO_EFFECTS_PER_CLIP
            && next_video_effect_id(&clip.video_effects).is_some();
        ui.add_enabled_ui(can_add, |ui| {
            ui.menu_button(
                t(state.language, "+ Add Effect", "+ エフェクトを追加"),
                |ui| {
                    ui.set_min_width(178.0);
                    ui.menu_button(t(state.language, "Color", "カラー"), |ui| {
                        if ui
                            .button(t(state.language, "Basic Correction", "基本補正"))
                            .clicked()
                        {
                            add_video_effect(
                                state,
                                clip,
                                VideoEffectKind::BrightnessContrast(
                                    BrightnessContrastEffect::default(),
                                ),
                            );
                            ui.close();
                        }
                    });
                    ui.menu_button(t(state.language, "Stylize", "スタイライズ"), |ui| {
                        if ui
                            .button(t(state.language, "Vignette", "ビネット"))
                            .clicked()
                        {
                            add_video_effect(
                                state,
                                clip,
                                VideoEffectKind::Vignette(VignetteEffect::default()),
                            );
                            ui.close();
                        }
                    });
                },
            )
            .response
            .on_hover_text(t(
                state.language,
                "Add a non-destructive effect after the current stack.",
                "現在のスタックの後に非破壊エフェクトを追加します。",
            ));
        });
    });

    if clip.video_effects.is_empty() {
        ui.label(
            RichText::new(t(
                state.language,
                "Build a non-destructive stack. Effects run from top to bottom.",
                "非破壊スタックを作成します。エフェクトは上から下へ適用されます。",
            ))
            .small()
            .color(Color32::from_rgb(137, 157, 171)),
        );
        return;
    }

    ui.label(
        RichText::new(t(state.language, "Applied top to bottom", "上から下へ適用"))
            .small()
            .color(Color32::from_rgb(137, 157, 171)),
    );
    let source_tick = inspector_source_tick(state, clip);
    for (index, node) in clip.video_effects.iter().enumerate() {
        color_effect_card(ui, state, clip, index, node, source_tick);
        ui.add_space(4.0);
    }
}

#[cfg(test)]
fn default_color_effect(id: VideoEffectId) -> VideoEffectNode {
    VideoEffectNode {
        id,
        enabled: true,
        kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
    }
}

fn add_video_effect(state: &mut EditorState, clip: &Clip, kind: VideoEffectKind) {
    if clip.video_effects.len() >= MAX_VIDEO_EFFECTS_PER_CLIP {
        return;
    }
    let mut effects = clip.video_effects.clone();
    let Some(id) = next_video_effect_id(&effects) else {
        return;
    };
    effects.push(VideoEffectNode {
        id,
        enabled: true,
        kind,
    });
    apply_track_header_edit(state, |timeline| {
        timeline.set_clip_video_effects(clip.id, effects)
    });
    state.active_color_effect = Some(id);
}

type VideoEffectScalars<'a> = [Option<(ColorParameter, &'a AnimatedScalar)>; 10];

fn video_effect_scalars(kind: &VideoEffectKind) -> VideoEffectScalars<'_> {
    match kind {
        VideoEffectKind::BrightnessContrast(effect) => [
            Some((ColorParameter::Temperature, &effect.temperature)),
            Some((ColorParameter::Tint, &effect.tint)),
            Some((ColorParameter::Saturation, &effect.saturation)),
            Some((ColorParameter::Exposure, &effect.exposure)),
            Some((ColorParameter::Contrast, &effect.contrast)),
            Some((ColorParameter::Highlights, &effect.highlights)),
            Some((ColorParameter::Shadows, &effect.shadows)),
            Some((ColorParameter::Whites, &effect.whites)),
            Some((ColorParameter::Blacks, &effect.blacks)),
            Some((ColorParameter::Brightness, &effect.brightness)),
        ],
        VideoEffectKind::Vignette(effect) => [
            Some((ColorParameter::VignetteAmount, &effect.amount)),
            Some((ColorParameter::VignetteMidpoint, &effect.midpoint)),
            Some((ColorParameter::VignetteFeather, &effect.feather)),
            Some((ColorParameter::VignetteCenterX, &effect.center_x)),
            Some((ColorParameter::VignetteCenterY, &effect.center_y)),
            None,
            None,
            None,
            None,
            None,
        ],
    }
}

fn next_video_effect_id(effects: &[VideoEffectNode]) -> Option<VideoEffectId> {
    let mut candidate = 1_u32;
    loop {
        let id = VideoEffectId(candidate);
        if effects.iter().all(|effect| effect.id != id) {
            return Some(id);
        }
        candidate = candidate.checked_add(1)?;
    }
}

fn color_effect_card(
    ui: &mut Ui,
    state: &mut EditorState,
    clip: &Clip,
    index: usize,
    node: &VideoEffectNode,
    source_tick: Tick,
) {
    let effect_id = node.id;
    let key_count = video_effect_scalars(&node.kind)
        .into_iter()
        .flatten()
        .map(|(_, scalar)| scalar.keyframes.len())
        .sum::<usize>();
    let active = active_color_effect_for_clip(state, clip) == Some(effect_id);
    let effect_label = match node.kind {
        VideoEffectKind::BrightnessContrast(_) => t(state.language, "Basic Correction", "基本補正"),
        VideoEffectKind::Vignette(_) => t(state.language, "Vignette", "ビネット"),
    };

    let card = Frame::new()
        .fill(if active {
            Color32::from_rgb(19, 30, 39)
        } else {
            Color32::from_rgb(17, 24, 31)
        })
        .stroke(Stroke::new(
            if active { 1.5 } else { 1.0 },
            if active {
                Color32::from_rgb(73, 160, 199)
            } else {
                Color32::from_rgb(39, 57, 68)
            },
        ))
        .inner_margin(egui::Margin::symmetric(8, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut enabled = node.enabled;
                let enabled_response = ui.checkbox(&mut enabled, "").on_hover_text(t(
                    state.language,
                    "Bypass this correction without deleting its settings.",
                    "設定を削除せずこの補正をバイパスします。",
                ));
                if enabled_response.changed() {
                    let mut effects = clip.video_effects.clone();
                    effects[index].enabled = enabled;
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_clip_video_effects(clip.id, effects)
                    });
                    state.active_color_effect = Some(effect_id);
                }
                ui.label(RichText::new(effect_label).strong());
                ui.label(
                    RichText::new(format!("{} · {key_count}", index + 1))
                        .small()
                        .color(Color32::from_rgb(111, 185, 211)),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    color_effect_actions_menu(ui, state, clip, index, node);
                });
            });
            ui.separator();
            match &node.kind {
                VideoEffectKind::BrightnessContrast(effect) => {
                    color_effect_group(
                        ui,
                        state,
                        clip,
                        index,
                        effect_id,
                        effect,
                        source_tick,
                        ColorCorrectionGroup::Color,
                    );
                    color_effect_group(
                        ui,
                        state,
                        clip,
                        index,
                        effect_id,
                        effect,
                        source_tick,
                        ColorCorrectionGroup::Light,
                    );
                    color_curves_group(ui, state, clip, effect_id, effect);
                }
                VideoEffectKind::Vignette(effect) => {
                    vignette_effect_controls(
                        ui,
                        state,
                        clip,
                        index,
                        effect_id,
                        effect,
                        source_tick,
                    );
                }
            }
        });
    if card.response.hovered() && ui.input(|input| input.pointer.primary_clicked()) {
        state.active_color_effect = Some(effect_id);
    }
}

#[derive(Clone, Copy)]
enum ColorCorrectionGroup {
    Color,
    Light,
}

fn color_effect_actions_menu(
    ui: &mut Ui,
    state: &mut EditorState,
    clip: &Clip,
    index: usize,
    node: &VideoEffectNode,
) {
    ui.menu_button("⋯", |ui| {
        if ui
            .add_enabled(
                index > 0,
                egui::Button::new(t(state.language, "Move earlier", "前へ移動")),
            )
            .clicked()
        {
            let mut effects = clip.video_effects.clone();
            effects.swap(index, index - 1);
            apply_track_header_edit(state, |timeline| {
                timeline.set_clip_video_effects(clip.id, effects)
            });
            state.active_color_effect = Some(node.id);
            ui.close();
        }
        if ui
            .add_enabled(
                index + 1 < clip.video_effects.len(),
                egui::Button::new(t(state.language, "Move later", "後へ移動")),
            )
            .clicked()
        {
            let mut effects = clip.video_effects.clone();
            effects.swap(index, index + 1);
            apply_track_header_edit(state, |timeline| {
                timeline.set_clip_video_effects(clip.id, effects)
            });
            state.active_color_effect = Some(node.id);
            ui.close();
        }
        let can_duplicate = clip.video_effects.len() < MAX_VIDEO_EFFECTS_PER_CLIP
            && next_video_effect_id(&clip.video_effects).is_some();
        if ui
            .add_enabled(
                can_duplicate,
                egui::Button::new(t(state.language, "Duplicate", "複製")),
            )
            .clicked()
        {
            let mut effects = clip.video_effects.clone();
            let id =
                next_video_effect_id(&effects).expect("enabled Duplicate has a free effect ID");
            let mut duplicate = node.clone();
            duplicate.id = id;
            effects.insert(index + 1, duplicate);
            apply_track_header_edit(state, |timeline| {
                timeline.set_clip_video_effects(clip.id, effects)
            });
            ui.close();
        }
        ui.separator();
        if ui.button(t(state.language, "Remove", "削除")).clicked() {
            let mut effects = clip.video_effects.clone();
            effects.remove(index);
            apply_track_header_edit(state, |timeline| {
                timeline.set_clip_video_effects(clip.id, effects)
            });
            ui.close();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn color_effect_group(
    ui: &mut Ui,
    state: &mut EditorState,
    clip: &Clip,
    index: usize,
    effect_id: VideoEffectId,
    effect: &BrightnessContrastEffect,
    source_tick: Tick,
    group: ColorCorrectionGroup,
) {
    let (english, japanese) = match group {
        ColorCorrectionGroup::Color => ("Color", "カラー"),
        ColorCorrectionGroup::Light => ("Light", "ライト"),
    };
    egui::CollapsingHeader::new(
        RichText::new(t(state.language, english, japanese))
            .size(11.0)
            .strong(),
    )
    .id_salt(("basic-correction-group", clip.id, effect_id, english))
    .default_open(matches!(group, ColorCorrectionGroup::Color))
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button(t(state.language, "Reset", "リセット"))
                    .clicked()
                {
                    let mut effects = clip.video_effects.clone();
                    let VideoEffectKind::BrightnessContrast(current) = &mut effects[index].kind
                    else {
                        return;
                    };
                    let defaults = BrightnessContrastEffect::default();
                    match group {
                        ColorCorrectionGroup::Color => {
                            current.temperature = defaults.temperature;
                            current.tint = defaults.tint;
                            current.saturation = defaults.saturation;
                        }
                        ColorCorrectionGroup::Light => {
                            current.exposure = defaults.exposure;
                            current.contrast = defaults.contrast;
                            current.highlights = defaults.highlights;
                            current.shadows = defaults.shadows;
                            current.whites = defaults.whites;
                            current.blacks = defaults.blacks;
                            current.brightness = defaults.brightness;
                        }
                    }
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_clip_video_effects(clip.id, effects)
                    });
                }
            });
        });
        match group {
            ColorCorrectionGroup::Color => {
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Temperature,
                    &effect.temperature,
                    source_tick,
                    t(state.language, "Temperature", "色温度"),
                    MIN_TEMPERATURE..=MAX_TEMPERATURE,
                    InspectorScrubUnit::Raw,
                    ColorRail::Temperature,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Tint,
                    &effect.tint,
                    source_tick,
                    t(state.language, "Tint", "色かぶり"),
                    MIN_TINT..=MAX_TINT,
                    InspectorScrubUnit::Raw,
                    ColorRail::Tint,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Saturation,
                    &effect.saturation,
                    source_tick,
                    t(state.language, "Saturation", "彩度"),
                    MIN_SATURATION..=MAX_SATURATION,
                    InspectorScrubUnit::Percent,
                    ColorRail::Saturation,
                );
            }
            ColorCorrectionGroup::Light => {
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Exposure,
                    &effect.exposure,
                    source_tick,
                    t(state.language, "Exposure", "露出"),
                    MIN_EXPOSURE..=MAX_EXPOSURE,
                    InspectorScrubUnit::Stops,
                    ColorRail::Neutral,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Contrast,
                    &effect.contrast,
                    source_tick,
                    t(state.language, "Contrast", "コントラスト"),
                    MIN_CONTRAST..=MAX_CONTRAST,
                    InspectorScrubUnit::Percent,
                    ColorRail::Neutral,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Highlights,
                    &effect.highlights,
                    source_tick,
                    t(state.language, "Highlights", "ハイライト"),
                    MIN_HIGHLIGHTS..=MAX_HIGHLIGHTS,
                    InspectorScrubUnit::Percent,
                    ColorRail::Neutral,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Shadows,
                    &effect.shadows,
                    source_tick,
                    t(state.language, "Shadows", "シャドウ"),
                    MIN_SHADOWS..=MAX_SHADOWS,
                    InspectorScrubUnit::Percent,
                    ColorRail::Neutral,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Whites,
                    &effect.whites,
                    source_tick,
                    t(state.language, "Whites", "白レベル"),
                    MIN_WHITES..=MAX_WHITES,
                    InspectorScrubUnit::Percent,
                    ColorRail::Neutral,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Blacks,
                    &effect.blacks,
                    source_tick,
                    t(state.language, "Blacks", "黒レベル"),
                    MIN_BLACKS..=MAX_BLACKS,
                    InspectorScrubUnit::Percent,
                    ColorRail::Neutral,
                );
                color_parameter_control(
                    ui,
                    state,
                    clip.id,
                    effect_id,
                    ColorParameter::Brightness,
                    &effect.brightness,
                    source_tick,
                    t(state.language, "Brightness", "明るさ"),
                    MIN_BRIGHTNESS..=MAX_BRIGHTNESS,
                    InspectorScrubUnit::Percent,
                    ColorRail::Neutral,
                );
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn vignette_effect_controls(
    ui: &mut Ui,
    state: &mut EditorState,
    clip: &Clip,
    index: usize,
    effect_id: VideoEffectId,
    effect: &VignetteEffect,
    source_tick: Tick,
) {
    let amount = effect.amount.evaluate(source_tick);
    let midpoint = effect.midpoint.evaluate(source_tick);
    let feather = effect.feather.evaluate(source_tick);
    let center_x = effect.center_x.evaluate(source_tick);
    let center_y = effect.center_y.evaluate(source_tick);
    vignette_preview_graph(ui, amount, midpoint, feather, center_x, center_y);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(t(state.language, "Shape", "形状"))
                .size(11.0)
                .strong(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .small_button(t(state.language, "Reset", "リセット"))
                .clicked()
            {
                let mut effects = clip.video_effects.clone();
                if let VideoEffectKind::Vignette(current) = &mut effects[index].kind {
                    *current = VignetteEffect::default();
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_clip_video_effects(clip.id, effects)
                    });
                }
            }
        });
    });
    color_parameter_control(
        ui,
        state,
        clip.id,
        effect_id,
        ColorParameter::VignetteAmount,
        &effect.amount,
        source_tick,
        t(state.language, "Amount", "適用量"),
        MIN_VIGNETTE_AMOUNT..=MAX_VIGNETTE_AMOUNT,
        InspectorScrubUnit::Percent,
        ColorRail::Neutral,
    );
    color_parameter_control(
        ui,
        state,
        clip.id,
        effect_id,
        ColorParameter::VignetteMidpoint,
        &effect.midpoint,
        source_tick,
        t(state.language, "Midpoint", "中間点"),
        MIN_VIGNETTE_MIDPOINT..=MAX_VIGNETTE_MIDPOINT,
        InspectorScrubUnit::Percent,
        ColorRail::Neutral,
    );
    color_parameter_control(
        ui,
        state,
        clip.id,
        effect_id,
        ColorParameter::VignetteFeather,
        &effect.feather,
        source_tick,
        t(state.language, "Feather", "ぼかし"),
        MIN_VIGNETTE_FEATHER..=MAX_VIGNETTE_FEATHER,
        InspectorScrubUnit::Percent,
        ColorRail::Neutral,
    );
    ui.add_space(2.0);
    ui.label(
        RichText::new(t(state.language, "Center", "中心"))
            .size(11.0)
            .strong(),
    );
    color_parameter_control(
        ui,
        state,
        clip.id,
        effect_id,
        ColorParameter::VignetteCenterX,
        &effect.center_x,
        source_tick,
        "X".to_owned(),
        MIN_VIGNETTE_CENTER..=MAX_VIGNETTE_CENTER,
        InspectorScrubUnit::Percent,
        ColorRail::Neutral,
    );
    color_parameter_control(
        ui,
        state,
        clip.id,
        effect_id,
        ColorParameter::VignetteCenterY,
        &effect.center_y,
        source_tick,
        "Y".to_owned(),
        MIN_VIGNETTE_CENTER..=MAX_VIGNETTE_CENTER,
        InspectorScrubUnit::Percent,
        ColorRail::Neutral,
    );
}

fn vignette_preview_graph(
    ui: &mut Ui,
    amount: f32,
    midpoint: f32,
    feather: f32,
    center_x: f32,
    center_y: f32,
) {
    let width = ui.available_width().clamp(176.0, 280.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 76.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, Color32::from_rgb(27, 35, 41));
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_rgb(53, 70, 80)),
        StrokeKind::Inside,
    );
    let center = Pos2::new(
        rect.center().x + center_x.clamp(-1.0, 1.0) * rect.width() * 0.34,
        rect.center().y + center_y.clamp(-1.0, 1.0) * rect.height() * 0.34,
    );
    let max_radius = rect.height() * 0.64;
    let inner = midpoint.clamp(0.0, 0.95) * max_radius;
    let outer = inner + feather.clamp(0.01, 1.0) * (max_radius - inner);
    for step in (1..=10).rev() {
        let progress = step as f32 / 10.0;
        let radius = inner + (outer - inner) * progress;
        let alpha = (amount.clamp(0.0, 1.0) * progress * progress * 150.0) as u8;
        painter.circle_stroke(
            center,
            radius,
            Stroke::new(2.0, Color32::from_black_alpha(alpha)),
        );
    }
    painter.line_segment(
        [center - Vec2::new(5.0, 0.0), center + Vec2::new(5.0, 0.0)],
        Stroke::new(1.0, Color32::from_rgb(104, 198, 224)),
    );
    painter.line_segment(
        [center - Vec2::new(0.0, 5.0), center + Vec2::new(0.0, 5.0)],
        Stroke::new(1.0, Color32::from_rgb(104, 198, 224)),
    );
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CurveChannel {
    #[default]
    Master,
    Red,
    Green,
    Blue,
}

#[derive(Clone, Copy, Debug, Default)]
struct CurveInspectorState {
    channel: CurveChannel,
    selected_point: Option<usize>,
}

fn color_curves_group(
    ui: &mut Ui,
    state: &mut EditorState,
    clip: &Clip,
    effect_id: VideoEffectId,
    effect: &BrightnessContrastEffect,
) {
    egui::CollapsingHeader::new(
        RichText::new(t(state.language, "Curves", "カーブ"))
            .size(11.0)
            .strong(),
    )
    .id_salt(("basic-correction-curves", clip.id, effect_id))
    .default_open(false)
    .show(ui, |ui| {
        let state_id = egui::Id::new(("curve-inspector-state", clip.id, effect_id));
        let mut curve_state = ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<CurveInspectorState>(state_id)
                .to_owned()
        });
        ui.horizontal_wrapped(|ui| {
            for (channel, label, color) in [
                (
                    CurveChannel::Master,
                    "Master",
                    Color32::from_rgb(230, 235, 240),
                ),
                (CurveChannel::Red, "R", Color32::from_rgb(237, 105, 105)),
                (CurveChannel::Green, "G", Color32::from_rgb(108, 201, 129)),
                (CurveChannel::Blue, "B", Color32::from_rgb(105, 162, 239)),
            ] {
                let selected = curve_state.channel == channel;
                let response = ui.add(
                    egui::Button::new(RichText::new(label).size(10.0).color(color))
                        .selected(selected)
                        .min_size(Vec2::new(28.0, 18.0)),
                );
                if response.clicked() {
                    curve_state.channel = channel;
                    curve_state.selected_point = None;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button(t(state.language, "Reset", "リセット"), |ui| {
                    if ui
                        .button(t(state.language, "Reset channel", "チャンネルをリセット"))
                        .clicked()
                    {
                        reset_curve_channel(state, clip.id, effect_id, curve_state.channel);
                        curve_state.selected_point = None;
                        ui.close();
                    }
                    if ui
                        .button(t(
                            state.language,
                            "Reset all curves",
                            "すべてのカーブをリセット",
                        ))
                        .clicked()
                    {
                        reset_all_curves(state, clip.id, effect_id);
                        curve_state.selected_point = None;
                        ui.close();
                    }
                });
            });
        });
        let curve = curve_for_channel(&effect.curves, curve_state.channel);
        color_curve_editor(ui, state, clip.id, effect_id, curve, &mut curve_state);
        ui.ctx()
            .data_mut(|data| data.insert_temp(state_id, curve_state));
    });
}

fn curve_for_channel(curves: &nle_timeline::RgbCurves, channel: CurveChannel) -> &ColorCurve {
    match channel {
        CurveChannel::Master => &curves.master,
        CurveChannel::Red => &curves.red,
        CurveChannel::Green => &curves.green,
        CurveChannel::Blue => &curves.blue,
    }
}

fn curve_for_channel_mut(
    curves: &mut nle_timeline::RgbCurves,
    channel: CurveChannel,
) -> &mut ColorCurve {
    match channel {
        CurveChannel::Master => &mut curves.master,
        CurveChannel::Red => &mut curves.red,
        CurveChannel::Green => &mut curves.green,
        CurveChannel::Blue => &mut curves.blue,
    }
}

fn mutate_color_curve(
    state: &mut EditorState,
    clip_id: ClipId,
    effect_id: VideoEffectId,
    channel: CurveChannel,
    mutate: impl FnOnce(&mut ColorCurve),
) -> bool {
    let Some(clip) = state.timeline.clip(clip_id) else {
        return false;
    };
    let mut effects = clip.video_effects.clone();
    let Some(node) = effects.iter_mut().find(|node| node.id == effect_id) else {
        return false;
    };
    let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
        return false;
    };
    mutate(curve_for_channel_mut(&mut effect.curves, channel));
    let generation = state.timeline.generation();
    state
        .timeline
        .set_clip_video_effects(clip_id, effects)
        .is_ok()
        && state.mark_changed_timeline_generation(generation)
}

fn reset_curve_channel(
    state: &mut EditorState,
    clip_id: ClipId,
    effect_id: VideoEffectId,
    channel: CurveChannel,
) {
    apply_track_header_edit(state, |timeline| {
        let Some(clip) = timeline.clip(clip_id) else {
            return Err(TimelineError::InvalidVideoEffect);
        };
        let mut effects = clip.video_effects.clone();
        let Some(node) = effects.iter_mut().find(|node| node.id == effect_id) else {
            return Err(TimelineError::InvalidVideoEffect);
        };
        let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
            return Err(TimelineError::InvalidVideoEffect);
        };
        *curve_for_channel_mut(&mut effect.curves, channel) = ColorCurve::default();
        timeline.set_clip_video_effects(clip_id, effects)
    });
}

fn reset_all_curves(state: &mut EditorState, clip_id: ClipId, effect_id: VideoEffectId) {
    apply_track_header_edit(state, |timeline| {
        let Some(clip) = timeline.clip(clip_id) else {
            return Err(TimelineError::InvalidVideoEffect);
        };
        let mut effects = clip.video_effects.clone();
        let Some(node) = effects.iter_mut().find(|node| node.id == effect_id) else {
            return Err(TimelineError::InvalidVideoEffect);
        };
        let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
            return Err(TimelineError::InvalidVideoEffect);
        };
        effect.curves = nle_timeline::RgbCurves::default();
        timeline.set_clip_video_effects(clip_id, effects)
    });
}

fn color_curve_editor(
    ui: &mut Ui,
    state: &mut EditorState,
    clip_id: ClipId,
    effect_id: VideoEffectId,
    curve: &ColorCurve,
    curve_state: &mut CurveInspectorState,
) {
    let graph_size = ui.available_width().clamp(176.0, 260.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(graph_size), Sense::click_and_drag());
    let channel_color = curve_channel_color(curve_state.channel);
    draw_curve_graph(ui, rect, curve, channel_color, curve_state.selected_point);
    if response.clicked() {
        response.request_focus();
    }
    let pointer = response.interact_pointer_pos();
    let hit = pointer.and_then(|point| curve_point_hit(curve, rect, point));
    if response.double_clicked() && hit.is_none() {
        if let Some(point) = pointer {
            let new_point = curve_graph_to_point(rect, point);
            if can_insert_curve_point(curve, new_point.x) {
                let selected_index = curve.points.partition_point(|point| point.x < new_point.x);
                apply_track_header_edit(state, |timeline| {
                    let Some(clip) = timeline.clip(clip_id) else {
                        return Err(TimelineError::InvalidVideoEffect);
                    };
                    let mut effects = clip.video_effects.clone();
                    let Some(node) = effects.iter_mut().find(|node| node.id == effect_id) else {
                        return Err(TimelineError::InvalidVideoEffect);
                    };
                    let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                        return Err(TimelineError::InvalidVideoEffect);
                    };
                    let curve = curve_for_channel_mut(&mut effect.curves, curve_state.channel);
                    curve.points.push(new_point);
                    curve
                        .points
                        .sort_by(|left, right| left.x.total_cmp(&right.x));
                    timeline.set_clip_video_effects(clip_id, effects)
                });
                curve_state.selected_point = Some(selected_index);
            }
        }
    } else if response.drag_started() {
        curve_state.selected_point = hit;
        if hit.is_some() {
            state.begin_timeline_history();
        }
    }
    if response.dragged()
        && let (Some(selected), Some(point)) = (curve_state.selected_point, pointer)
    {
        let proposed = curve_graph_to_point(rect, point);
        let point_count = curve.points.len();
        if selected < point_count {
            let x = constrained_curve_point_x(curve, selected, proposed.x);
            let y = proposed.y.clamp(0.0, 1.0);
            let _ = mutate_color_curve(state, clip_id, effect_id, curve_state.channel, |curve| {
                if let Some(point) = curve.points.get_mut(selected) {
                    point.x = x;
                    point.y = y;
                }
            });
        }
    }
    if response.drag_stopped() {
        state.commit_timeline_history();
    }
    let remove_selected = curve_state
        .selected_point
        .is_some_and(|selected| selected > 0 && selected + 1 < curve.points.len());
    let delete_pressed = response.has_focus()
        && ui.input(|input| {
            input.key_pressed(egui::Key::Delete) || input.key_pressed(egui::Key::Backspace)
        });
    if delete_pressed && remove_selected {
        let selected = curve_state.selected_point.expect("validated above");
        apply_track_header_edit(state, |timeline| {
            let Some(clip) = timeline.clip(clip_id) else {
                return Err(TimelineError::InvalidVideoEffect);
            };
            let mut effects = clip.video_effects.clone();
            let Some(node) = effects.iter_mut().find(|node| node.id == effect_id) else {
                return Err(TimelineError::InvalidVideoEffect);
            };
            let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                return Err(TimelineError::InvalidVideoEffect);
            };
            curve_for_channel_mut(&mut effect.curves, curve_state.channel)
                .points
                .remove(selected);
            timeline.set_clip_video_effects(clip_id, effects)
        });
        curve_state.selected_point = None;
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                remove_selected,
                egui::Button::new(t(state.language, "Remove point", "ポイントを削除")).small(),
            )
            .clicked()
        {
            let selected = curve_state
                .selected_point
                .expect("enabled only for internal points");
            apply_track_header_edit(state, |timeline| {
                let Some(clip) = timeline.clip(clip_id) else {
                    return Err(TimelineError::InvalidVideoEffect);
                };
                let mut effects = clip.video_effects.clone();
                let Some(node) = effects.iter_mut().find(|node| node.id == effect_id) else {
                    return Err(TimelineError::InvalidVideoEffect);
                };
                let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                    return Err(TimelineError::InvalidVideoEffect);
                };
                curve_for_channel_mut(&mut effect.curves, curve_state.channel)
                    .points
                    .remove(selected);
                timeline.set_clip_video_effects(clip_id, effects)
            });
            curve_state.selected_point = None;
        }
        if let Some(selected) = curve_state
            .selected_point
            .and_then(|index| curve.points.get(index))
        {
            ui.label(
                RichText::new(format!("In {:.3}  Out {:.3}", selected.x, selected.y))
                    .small()
                    .color(Color32::from_rgb(94, 168, 232)),
            );
        } else {
            ui.label(
                RichText::new(t(
                    state.language,
                    "Double-click to add a point",
                    "ダブルクリックでポイントを追加",
                ))
                .small()
                .color(Color32::from_rgb(137, 157, 171)),
            );
        }
    });
}

fn curve_channel_color(channel: CurveChannel) -> Color32 {
    match channel {
        CurveChannel::Master => Color32::from_rgb(229, 234, 239),
        CurveChannel::Red => Color32::from_rgb(239, 103, 103),
        CurveChannel::Green => Color32::from_rgb(100, 205, 130),
        CurveChannel::Blue => Color32::from_rgb(100, 164, 242),
    }
}

fn draw_curve_graph(
    ui: &Ui,
    rect: Rect,
    curve: &ColorCurve,
    color: Color32,
    selected: Option<usize>,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, Color32::from_rgb(16, 23, 30));
    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, Color32::from_rgb(58, 75, 87)),
        StrokeKind::Inside,
    );
    for step in 1..4 {
        let fraction = step as f32 / 4.0;
        let x = egui::lerp(rect.left()..=rect.right(), fraction);
        let y = egui::lerp(rect.top()..=rect.bottom(), fraction);
        let grid = Stroke::new(1.0, Color32::from_rgb(42, 54, 64));
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            grid,
        );
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            grid,
        );
    }
    painter.line_segment(
        [rect.left_bottom(), rect.right_top()],
        Stroke::new(1.0, Color32::from_rgb(104, 118, 129)),
    );
    let samples = 64;
    let mut previous = curve_point_to_graph(
        rect,
        CurvePoint {
            x: 0.0,
            y: curve_sample(curve, 0.0),
        },
    );
    for sample in 1..=samples {
        let x = sample as f32 / samples as f32;
        let next = curve_point_to_graph(
            rect,
            CurvePoint {
                x,
                y: curve_sample(curve, x),
            },
        );
        painter.line_segment([previous, next], Stroke::new(2.0, color));
        previous = next;
    }
    for (index, point) in curve.points.iter().copied().enumerate() {
        let point = curve_point_to_graph(rect, point);
        if selected == Some(index) {
            painter.circle_stroke(point, 7.0, Stroke::new(2.0, color.gamma_multiply(0.75)));
        }
        painter.circle_filled(point, 4.0, color);
        painter.circle_stroke(point, 4.0, Stroke::new(1.0, Color32::from_rgb(16, 23, 30)));
    }
}

fn curve_sample(curve: &ColorCurve, x: f32) -> f32 {
    curve.sample(x)
}

fn curve_point_to_graph(rect: Rect, point: CurvePoint) -> Pos2 {
    Pos2::new(
        egui::lerp(rect.left()..=rect.right(), point.x.clamp(0.0, 1.0)),
        egui::lerp(rect.bottom()..=rect.top(), point.y.clamp(0.0, 1.0)),
    )
}

fn curve_graph_to_point(rect: Rect, point: Pos2) -> CurvePoint {
    CurvePoint {
        x: ((point.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0),
        y: ((rect.bottom() - point.y) / rect.height().max(1.0)).clamp(0.0, 1.0),
    }
}

fn curve_point_hit(curve: &ColorCurve, rect: Rect, pointer: Pos2) -> Option<usize> {
    curve
        .points
        .iter()
        .enumerate()
        .filter_map(|(index, point)| {
            (curve_point_to_graph(rect, *point).distance(pointer) <= 9.0).then_some(index)
        })
        .min_by(|left, right| {
            curve_point_to_graph(rect, curve.points[*left])
                .distance(pointer)
                .total_cmp(&curve_point_to_graph(rect, curve.points[*right]).distance(pointer))
        })
}

fn can_insert_curve_point(curve: &ColorCurve, x: f32) -> bool {
    curve.points.len() < MAX_COLOR_CURVE_POINTS
        && curve
            .points
            .iter()
            .all(|point| (point.x - x).abs() >= 1.0 / 255.0)
}

fn constrained_curve_point_x(curve: &ColorCurve, selected: usize, proposed: f32) -> f32 {
    if selected == 0 {
        return 0.0;
    }
    if selected + 1 >= curve.points.len() {
        return 1.0;
    }
    let minimum = curve.points[selected - 1].x + 1.0 / 255.0;
    let maximum = curve.points[selected + 1].x - 1.0 / 255.0;
    if minimum > maximum {
        curve.points[selected].x
    } else {
        proposed.clamp(minimum, maximum)
    }
}

#[allow(clippy::too_many_arguments)]
fn color_parameter_control(
    ui: &mut Ui,
    state: &mut EditorState,
    clip_id: ClipId,
    effect_id: VideoEffectId,
    parameter: ColorParameter,
    scalar: &AnimatedScalar,
    source_tick: Tick,
    label: String,
    range: std::ops::RangeInclusive<f32>,
    unit: InspectorScrubUnit,
    rail: ColorRail,
) {
    let current_key = scalar
        .keyframes
        .binary_search_by_key(&source_tick, |key| key.source_tick)
        .ok()
        .and_then(|index| scalar.keyframes.get(index));
    let mut value = scalar.evaluate(source_tick);
    let slider_response = ui
        .horizontal(|ui| {
            let diamond = ui
                .small_button(if current_key.is_some() { "◆" } else { "◇" })
                .on_hover_text(t(
                    state.language,
                    if current_key.is_some() {
                        "Remove the keyframe at the playhead"
                    } else {
                        "Add a keyframe at the playhead"
                    },
                    if current_key.is_some() {
                        "再生ヘッド位置のキーフレームを削除"
                    } else {
                        "再生ヘッド位置にキーフレームを追加"
                    },
                ));
            if diamond.clicked() {
                if current_key.is_some() {
                    apply_track_header_edit(state, |timeline| {
                        timeline
                            .remove_color_keyframe(clip_id, effect_id, parameter, source_tick)
                            .map(|_| ())
                    });
                } else {
                    apply_track_header_edit(state, |timeline| {
                        timeline.set_color_keyframe(
                            clip_id,
                            effect_id,
                            parameter,
                            source_tick,
                            value,
                            KeyframeInterpolation::Linear,
                        )
                    });
                }
            }
            ui.add_sized(
                [76.0, 18.0],
                egui::Label::new(RichText::new(label).size(11.0)),
            );
            let rail_width = (ui.available_width() - 72.0).clamp(54.0, 136.0);
            let rail_response = ui.add_sized(
                [rail_width, 18.0],
                egui::Slider::new(&mut value, range.clone()).show_value(false),
            );
            paint_color_parameter_rail(ui, rail_response.rect, rail);
            let scrub_response = inspector_scrub_numeric_value(ui, &mut value, range, unit);
            rail_response.union(scrub_response)
        })
        .inner;
    let interpolation = current_key
        .map(|key| key.interpolation)
        .unwrap_or(KeyframeInterpolation::Linear);
    mixer_live_edit(state, &slider_response, |timeline| {
        if scalar.keyframes.is_empty() {
            timeline.set_color_parameter(clip_id, effect_id, parameter, value)
        } else {
            timeline.set_color_keyframe(
                clip_id,
                effect_id,
                parameter,
                source_tick,
                value,
                interpolation,
            )
        }
    });

    if let Some(key) = current_key {
        let mut selected = key.interpolation;
        ui.horizontal(|ui| {
            ui.add_space(24.0);
            ui.label(t(state.language, "Interpolation", "補間"));
            egui::ComboBox::from_id_salt((
                "color-interpolation",
                clip_id,
                effect_id,
                parameter as u8,
            ))
            .selected_text(keyframe_interpolation_label(state.language, selected))
            .show_ui(ui, |ui| {
                for interpolation in COLOR_KEYFRAME_INTERPOLATIONS {
                    ui.selectable_value(
                        &mut selected,
                        interpolation,
                        keyframe_interpolation_label(state.language, interpolation),
                    )
                    .on_hover_text(keyframe_interpolation_tooltip(
                        state.language,
                        interpolation,
                    ));
                }
            });
        });
        if selected != key.interpolation {
            apply_track_header_edit(state, |timeline| {
                timeline.set_color_keyframe(
                    clip_id,
                    effect_id,
                    parameter,
                    source_tick,
                    key.value,
                    selected,
                )
            });
        }
    }
}

#[derive(Clone, Copy)]
enum ColorRail {
    Temperature,
    Tint,
    Saturation,
    Neutral,
}

fn paint_color_parameter_rail(ui: &Ui, rect: Rect, rail: ColorRail) {
    let y = rect.center().y;
    let left = rect.left() + 8.0;
    let right = rect.right() - 8.0;
    let midpoint = (left + right) * 0.5;
    let colors = match rail {
        ColorRail::Temperature => (
            Color32::from_rgb(72, 154, 219),
            Color32::from_rgb(154, 160, 166),
            Color32::from_rgb(225, 164, 83),
        ),
        ColorRail::Tint => (
            Color32::from_rgb(85, 172, 119),
            Color32::from_rgb(154, 160, 166),
            Color32::from_rgb(202, 104, 174),
        ),
        ColorRail::Saturation => (
            Color32::from_rgb(132, 141, 148),
            Color32::from_rgb(83, 164, 216),
            Color32::from_rgb(83, 190, 226),
        ),
        ColorRail::Neutral => (
            Color32::from_rgb(95, 108, 117),
            Color32::from_rgb(132, 145, 153),
            Color32::from_rgb(95, 108, 117),
        ),
    };
    let mut gradient = egui::Mesh::default();
    for (start, end, start_color, end_color) in [
        (left, midpoint, colors.0, colors.1),
        (midpoint, right, colors.1, colors.2),
    ] {
        let first = gradient.vertices.len() as u32;
        for (point, color) in [
            (Pos2::new(start, y - 1.0), start_color),
            (Pos2::new(end, y - 1.0), end_color),
            (Pos2::new(end, y + 1.0), end_color),
            (Pos2::new(start, y + 1.0), start_color),
        ] {
            gradient.colored_vertex(point, color);
        }
        gradient.add_triangle(first, first + 1, first + 2);
        gradient.add_triangle(first, first + 2, first + 3);
    }
    ui.painter().add(gradient);
    ui.painter().line_segment(
        [Pos2::new(midpoint, y - 3.0), Pos2::new(midpoint, y + 3.0)],
        Stroke::new(1.0, colors.1),
    );
}

const COLOR_KEYFRAME_INTERPOLATIONS: [KeyframeInterpolation; 5] = [
    KeyframeInterpolation::Linear,
    KeyframeInterpolation::Smooth,
    KeyframeInterpolation::EaseIn,
    KeyframeInterpolation::EaseOut,
    KeyframeInterpolation::Hold,
];

fn keyframe_interpolation_label(
    language: Language,
    interpolation: KeyframeInterpolation,
) -> String {
    match interpolation {
        KeyframeInterpolation::Linear => t(language, "Linear", "リニア"),
        KeyframeInterpolation::Smooth => t(language, "Smooth", "スムーズ"),
        KeyframeInterpolation::EaseIn => t(language, "Ease In", "イーズイン"),
        KeyframeInterpolation::EaseOut => t(language, "Ease Out", "イーズアウト"),
        KeyframeInterpolation::Hold => t(language, "Hold", "ホールド"),
    }
}

fn keyframe_interpolation_tooltip(
    language: Language,
    interpolation: KeyframeInterpolation,
) -> &'static str {
    match (language, interpolation) {
        (Language::English, KeyframeInterpolation::Linear) => {
            "Change at a constant rate between keyframes."
        }
        (Language::Japanese, KeyframeInterpolation::Linear) => {
            "キーフレーム間を一定の速度で変化させます。"
        }
        (Language::English, KeyframeInterpolation::Smooth) => {
            "Gently accelerate and decelerate around both keyframes."
        }
        (Language::Japanese, KeyframeInterpolation::Smooth) => {
            "両方のキーフレーム付近で滑らかに加速・減速します。"
        }
        (Language::English, KeyframeInterpolation::EaseIn) => {
            "Start slowly, then accelerate toward the next keyframe."
        }
        (Language::Japanese, KeyframeInterpolation::EaseIn) => {
            "ゆっくり始まり、次のキーフレームに向かって加速します。"
        }
        (Language::English, KeyframeInterpolation::EaseOut) => {
            "Start quickly, then decelerate into the next keyframe."
        }
        (Language::Japanese, KeyframeInterpolation::EaseOut) => {
            "速く始まり、次のキーフレームに向かって減速します。"
        }
        (Language::English, KeyframeInterpolation::Hold) => {
            "Keep this value unchanged until the next keyframe."
        }
        (Language::Japanese, KeyframeInterpolation::Hold) => {
            "次のキーフレームまでこの値を維持します。"
        }
    }
}

fn sizing_mode_label(language: Language, mode: nle_timeline::ClipSizingMode) -> &'static str {
    match (language, mode) {
        (Language::English, nle_timeline::ClipSizingMode::Fit) => "Fit",
        (Language::Japanese, nle_timeline::ClipSizingMode::Fit) => "フィット",
        (Language::English, nle_timeline::ClipSizingMode::Fill) => "Fill",
        (Language::Japanese, nle_timeline::ClipSizingMode::Fill) => "塗りつぶし",
        (Language::English, nle_timeline::ClipSizingMode::Stretch) => "Stretch",
        (Language::Japanese, nle_timeline::ClipSizingMode::Stretch) => "引き伸ばし",
        (Language::English, nle_timeline::ClipSizingMode::Original) => "Original Pixels",
        (Language::Japanese, nle_timeline::ClipSizingMode::Original) => "元のピクセル",
    }
}

fn sizing_mode_tooltip(language: Language, mode: nle_timeline::ClipSizingMode) -> &'static str {
    match (language, mode) {
        (Language::English, nle_timeline::ClipSizingMode::Fit) => {
            "Show the entire source while preserving its aspect ratio."
        }
        (Language::Japanese, nle_timeline::ClipSizingMode::Fit) => {
            "アスペクト比を保ってソース全体を表示します。"
        }
        (Language::English, nle_timeline::ClipSizingMode::Fill) => {
            "Cover the project frame while preserving its aspect ratio."
        }
        (Language::Japanese, nle_timeline::ClipSizingMode::Fill) => {
            "アスペクト比を保ってプロジェクト画面を覆います。"
        }
        (Language::English, nle_timeline::ClipSizingMode::Stretch) => {
            "Fill the project frame, independently scaling both axes."
        }
        (Language::Japanese, nle_timeline::ClipSizingMode::Stretch) => {
            "両方の軸を個別に拡大してプロジェクト画面を満たします。"
        }
        (Language::English, nle_timeline::ClipSizingMode::Original) => {
            "Keep cropped source pixels at their original size."
        }
        (Language::Japanese, nle_timeline::ClipSizingMode::Original) => {
            "クロップ後のソースを元のピクセルサイズで表示します。"
        }
    }
}

fn crop_edge_max(opposing_edge: f32) -> f32 {
    (nle_timeline::ClipTransform::MAX_CROP_TOTAL - opposing_edge).max(0.0)
}

fn details(ui: &mut Ui, state: &mut EditorState) {
    right_sidebar_tabs(ui, state);
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt(state.right_sidebar_tab.scroll_id())
        .auto_shrink([false, false])
        .show(ui, |ui| match state.right_sidebar_tab {
            RightSidebarTab::Inspector => inspector_details(ui, state),
            RightSidebarTab::Audio => audio_details(ui, state),
            RightSidebarTab::Color => color_details(ui, state),
            RightSidebarTab::Effects => transition_details(ui, state),
            RightSidebarTab::Media => metadata_details(ui, state),
        });
}

fn right_sidebar_tabs(ui: &mut Ui, state: &mut EditorState) {
    let accent = Color32::from_rgb(83, 190, 226);
    ui.columns(5, |columns| {
        for (column, tab) in columns.iter_mut().zip(RightSidebarTab::ALL) {
            let selected = state.right_sidebar_tab == tab;
            let response = column
                .add_sized(
                    [column.available_width(), 26.0],
                    egui::Button::new(format!("{} {}", tab.icon(), tab.label(state.language)))
                        .selected(selected)
                        .frame(false),
                )
                .on_hover_text(tab.tooltip(state.language));
            if response.clicked() {
                select_right_sidebar_tab(state, tab);
            }
            if selected {
                let rect = response.rect;
                column.painter().line_segment(
                    [
                        Pos2::new(rect.left() + 5.0, rect.bottom() - 1.0),
                        Pos2::new(rect.right() - 5.0, rect.bottom() - 1.0),
                    ],
                    Stroke::new(2.0, accent),
                );
            }
        }
    });
}

fn select_right_sidebar_tab(state: &mut EditorState, tab: RightSidebarTab) {
    state.right_sidebar_tab = tab;
}

fn focus_audio_track_in_audio_tab(state: &mut EditorState, track_id: TrackId) {
    if !state
        .timeline
        .track(track_id)
        .is_some_and(|track| track.kind == TrackKind::Audio)
    {
        return;
    }
    // The clicked header is already visible, so keep the user's timeline
    // scroll position while sharing focus with the Undertow mixer.
    state.undertow_track = Some(track_id);
    state.selected_timeline_clip = None;
    state.selected_title = None;
    select_right_sidebar_tab(state, RightSidebarTab::Audio);
}

fn inspector_details(ui: &mut Ui, state: &mut EditorState) {
    active_preview_inspector(ui, state);
    title_inspector(ui, state);
    audio_crossfade_inspector(ui, state);
    clip_inspector(ui, state);
}

fn active_preview_inspector(ui: &mut Ui, state: &EditorState) {
    if !state.active_preview_diagnostics.iter().any(Option::is_some) {
        return;
    }
    Frame::new()
        .fill(Color32::from_rgb(17, 28, 32))
        .stroke(Stroke::new(1.0, Color32::from_rgb(43, 79, 85)))
        .inner_margin(egui::Margin::symmetric(8, 7))
        .show(ui, |ui| {
            ui.label(
                RichText::new(t(state.language, "Active preview", "アクティブプレビュー"))
                    .small()
                    .strong(),
            );
            for (layer, diagnostic) in state
                .active_preview_diagnostics
                .iter()
                .enumerate()
                .filter_map(|(layer, diagnostic)| diagnostic.map(|diagnostic| (layer, diagnostic)))
            {
                ui.add_space(3.0);
                ui.label(
                    RichText::new(format!(
                        "{} {} · {} {}",
                        t(state.language, "Layer", "レイヤー"),
                        layer + 1,
                        t(state.language, "Media", "メディア"),
                        diagnostic.media_id,
                    ))
                    .small()
                    .strong()
                    .color(Color32::from_rgb(132, 170, 174)),
                );
                metadata_row(
                    ui,
                    &t(state.language, "Source", "ソース"),
                    active_preview_source_label(state.language, diagnostic.source_kind),
                );
                let decoder = diagnostic
                    .decoder_backend
                    .map(|backend| active_preview_decoder_label(state.language, backend))
                    .unwrap_or_else(|| match state.language {
                        Language::English => "Not observed",
                        Language::Japanese => "未確認",
                    });
                metadata_row(ui, &t(state.language, "Decoder", "デコーダー"), decoder);
                let quality = format!(
                    "{} → {} · {}×{}",
                    preview_quality_option_label(state.language, diagnostic.selected_quality),
                    preview_quality_option_label(state.language, diagnostic.resolved_quality),
                    diagnostic.width,
                    diagnostic.height,
                );
                metadata_row(ui, &t(state.language, "Quality", "品質"), &quality);
                let fallback = active_preview_fallback_status_label(state.language, diagnostic);
                metadata_row(
                    ui,
                    &t(state.language, "Fallback", "フォールバック"),
                    fallback,
                );
            }
        });
    ui.add_space(6.0);
}

fn active_preview_source_label(
    language: Language,
    source: ActivePreviewSourceKind,
) -> &'static str {
    match (language, source) {
        (Language::English, ActivePreviewSourceKind::OriginalSource) => "Original source",
        (Language::Japanese, ActivePreviewSourceKind::OriginalSource) => "元のソース",
        (Language::English, ActivePreviewSourceKind::InternalScrubPreview) => {
            "Internal scrub preview"
        }
        (Language::Japanese, ActivePreviewSourceKind::InternalScrubPreview) => {
            "内部スクラブプレビュー"
        }
    }
}

fn active_preview_decoder_label(
    language: Language,
    backend: ActivePreviewDecoderBackend,
) -> &'static str {
    match (language, backend) {
        (_, ActivePreviewDecoderBackend::Software) => "Software",
        (_, ActivePreviewDecoderBackend::IntelQuickSync) => "Intel Quick Sync",
        (_, ActivePreviewDecoderBackend::NvidiaCuvid) => "NVIDIA CUVID",
        (_, ActivePreviewDecoderBackend::AppleVideoToolbox) => "Apple VideoToolbox",
        (_, ActivePreviewDecoderBackend::WindowsD3d11va) => "Windows D3D11VA",
        (_, ActivePreviewDecoderBackend::WindowsDxva2) => "Windows DXVA2",
    }
}

fn active_preview_fallback_label(
    language: Language,
    reason: ActivePreviewFallbackReason,
) -> &'static str {
    match (language, reason) {
        (Language::English, ActivePreviewFallbackReason::ForcedSoftware) => "Forced software",
        (Language::Japanese, ActivePreviewFallbackReason::ForcedSoftware) => "ソフトウェアを強制",
        (Language::English, ActivePreviewFallbackReason::HardwareUnavailable) => {
            "Hardware unavailable"
        }
        (Language::Japanese, ActivePreviewFallbackReason::HardwareUnavailable) => {
            "ハードウェアを利用できません"
        }
        (Language::English, ActivePreviewFallbackReason::HardwareDecodeFailed) => {
            "Hardware decode failed"
        }
        (Language::Japanese, ActivePreviewFallbackReason::HardwareDecodeFailed) => {
            "ハードウェアデコードに失敗"
        }
    }
}

fn active_preview_fallback_status_label(
    language: Language,
    diagnostic: ActivePreviewDiagnostic,
) -> &'static str {
    if let Some(reason) = diagnostic.fallback_reason {
        return active_preview_fallback_label(language, reason);
    }
    let observed_without_fallback = diagnostic
        .decoder_backend
        .is_some_and(|backend| backend != ActivePreviewDecoderBackend::Software);
    match (language, observed_without_fallback) {
        (Language::English, true) => "Not needed",
        (Language::Japanese, true) => "不要",
        (Language::English, false) => "Not observed",
        (Language::Japanese, false) => "未確認",
    }
}

fn audio_details(ui: &mut Ui, state: &mut EditorState) {
    let selected_audio = state.selected_timeline_clip.and_then(|clip_id| {
        state
            .timeline
            .clip(clip_id)
            .filter(|clip| {
                state
                    .timeline
                    .track(clip.track_id)
                    .is_some_and(|track| track.kind == TrackKind::Audio)
            })
            .cloned()
    });
    // Timeline selection owns the Audio tab. Fall back to the Media Pool only when no audio clip
    // is selected so the waveform and the controls can never describe different sources.
    let selected_id = selected_audio
        .as_ref()
        .map(|clip| clip.media.0)
        .or(state.selected_media);
    let focused_track = selected_audio
        .as_ref()
        .map(|clip| clip.track_id)
        .or_else(|| {
            state.undertow_track.filter(|track_id| {
                state
                    .timeline
                    .track(*track_id)
                    .is_some_and(|track| track.kind == TrackKind::Audio)
            })
        })
        .or_else(|| {
            state
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Audio)
                .map(|track| track.id)
        });
    let panel_state = analysis_panel_state(state, selected_id);
    panel_title(
        ui,
        &t(state.language, "Audio", "オーディオ"),
        &analysis_panel_status(state.language, panel_state, state.playing),
    );
    if ui
        .checkbox(
            &mut state.force_software_decode,
            t(
                state.language,
                "Force software decoding",
                "ソフトウェアデコードを強制",
            ),
        )
        .on_hover_text(t(
            state.language,
            "Disable GPU video decoding for compatibility or diagnosis",
            "互換性確認や診断のためGPU動画デコードを無効化",
        ))
        .changed()
    {
        state.emit(EditorAction::SetForceSoftwareDecode(
            state.force_software_decode,
        ));
    }
    ui.label(
        RichText::new(t(state.language, "Audio meters", "オーディオメーター"))
            .small()
            .strong(),
    );
    let meter_rect = ui.allocate_space(Vec2::new(ui.available_width(), 72.0)).1;
    draw_audio_meters(ui.painter(), meter_rect, active_audio_levels(state));
    if let Some(error) = &state.audio_output_error {
        ui.label(
            RichText::new(t(
                state.language,
                "Audio output unavailable",
                "オーディオ出力を利用できません",
            ))
            .small()
            .color(Color32::from_rgb(226, 125, 125)),
        )
        .on_hover_text(error);
    }
    ui.label(
        RichText::new(t(state.language, "Waveform", "波形"))
            .small()
            .strong(),
    );
    let wave_rect = ui.allocate_space(Vec2::new(ui.available_width(), 52.0)).1;
    let painter = ui.painter();
    painter.rect_filled(wave_rect, 2.0, Color32::from_rgb(15, 20, 27));
    painter.line_segment(
        [
            Pos2::new(wave_rect.left() + 5.0, wave_rect.center().y),
            Pos2::new(wave_rect.right() - 5.0, wave_rect.center().y),
        ],
        Stroke::new(1.0, Color32::from_rgb(47, 65, 78)),
    );
    if let Some(waveform) = selected_id.and_then(|id| state.waveforms.get(&id)) {
        draw_waveform(
            painter,
            wave_rect.shrink2(Vec2::new(4.0, 3.0)),
            &waveform.peaks,
            Color32::from_rgb(92, 214, 157),
        );
    } else if let Some(error) = selected_id.and_then(|id| state.waveform_errors.get(&id)) {
        painter.text(
            wave_rect.center(),
            egui::Align2::CENTER_CENTER,
            t(state.language, "No audio waveform", "オーディオ波形なし"),
            FontId::proportional(11.0),
            Color32::from_rgb(215, 126, 126),
        );
        ui.interact(
            wave_rect,
            ui.id().with("analysis-waveform-error"),
            Sense::hover(),
        )
        .on_hover_text(error);
    } else if panel_state == AnalysisPanelState::Analyzing {
        painter.text(
            wave_rect.center(),
            egui::Align2::CENTER_CENTER,
            t(state.language, "Analyzing…", "解析中…"),
            FontId::proportional(11.0),
            Color32::from_rgb(137, 153, 165),
        );
    } else {
        let message = match panel_state {
            AnalysisPanelState::NoSelection => t(state.language, "Select media", "メディアを選択"),
            AnalysisPanelState::AwaitingPlacement => t(
                state.language,
                "Place on timeline to analyze",
                "タイムラインに配置して解析",
            ),
            AnalysisPanelState::Ready | AnalysisPanelState::Offline => {
                t(state.language, "No audio waveform", "オーディオ波形なし")
            }
            AnalysisPanelState::Analyzing => unreachable!(),
        };
        painter.text(
            wave_rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            FontId::proportional(11.0),
            Color32::from_rgb(137, 153, 165),
        );
    }
    ui.separator();
    if let Some(clip) = selected_audio.as_ref() {
        ui.label(
            RichText::new(t(
                state.language,
                "Signal path · Clip Effects → Track Effects",
                "信号経路 · クリップエフェクト → トラックエフェクト",
            ))
            .small()
            .strong()
            .color(Color32::from_rgb(132, 170, 174)),
        );
        audio_clip_controls(ui, state, clip);
        ui.add_space(6.0);
    }
    if let Some(track_id) = focused_track {
        let track_label = state
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .position(|track| track.id == track_id)
            .map_or_else(|| "Audio".to_owned(), |index| format!("A{}", index + 1));
        Frame::new()
            .fill(Color32::from_rgb(17, 28, 32))
            .stroke(Stroke::new(1.0, Color32::from_rgb(43, 79, 85)))
            .inner_margin(egui::Margin::symmetric(8, 7))
            .show(ui, |ui| {
                ui.label(RichText::new(track_label).strong());
                audio_effects_rack(ui, state, AudioEffectsScope::Track(track_id));
            });
    }
    if selected_audio.is_none() {
        ui.label(
            RichText::new(t(
                state.language,
                "Select an audio clip to adjust volume and channels.",
                "オーディオクリップを選択して音量とチャンネルを調整します。",
            ))
            .small()
            .color(Color32::from_rgb(133, 150, 165)),
        );
    }
}

fn color_details(ui: &mut Ui, state: &mut EditorState) {
    panel_title(ui, &t(state.language, "Color", "カラー"), "");
    let selected_video = state.selected_timeline_clip.and_then(|clip_id| {
        state
            .timeline
            .clip(clip_id)
            .filter(|clip| {
                state
                    .timeline
                    .track(clip.track_id)
                    .is_some_and(|track| track.kind == TrackKind::Video)
            })
            .cloned()
    });
    if let Some(clip) = selected_video {
        color_correction_inspector(ui, state, &clip);
    } else {
        ui.label(
            RichText::new(t(
                state.language,
                "Select a video clip to add or adjust color correction.",
                "ビデオクリップを選択してカラー補正を追加または調整します。",
            ))
            .small()
            .color(Color32::from_rgb(133, 150, 165)),
        );
    }
}

fn metadata_details(ui: &mut Ui, state: &mut EditorState) {
    let selected_id = state.selected_media;
    let panel_state = analysis_panel_state(state, selected_id);
    ui.label(
        RichText::new(t(state.language, "Metadata", "メタデータ"))
            .small()
            .strong(),
    );
    if let Some(item) = state.selected() {
        let metadata = state.media_metadata.get(&item.id);
        if let Some(error) = state.media_errors.get(&item.id) {
            ui.label(
                RichText::new(t(
                    state.language,
                    "Media is offline or unreadable",
                    "メディアがオフラインまたは読み取り不能です",
                ))
                .small()
                .color(Color32::from_rgb(255, 126, 224)),
            )
            .on_hover_text(error);
        }
        ui.vertical(|ui| {
            metadata_row(ui, &t(state.language, "Name", "名前"), &item.display_name);
            metadata_row(
                ui,
                &t(state.language, "Kind", "種類"),
                kind_name(state.language, item.kind),
            );
            metadata_row(
                ui,
                &t(state.language, "Duration", "長さ"),
                &metadata
                    .and_then(|value| value.duration_seconds)
                    .map(format_duration)
                    .unwrap_or_else(|| "—".into()),
            );
            metadata_row(
                ui,
                &t(state.language, "Video", "映像"),
                &format_video_metadata(metadata),
            );
            metadata_row(
                ui,
                &t(state.language, "Frame rate", "フレームレート"),
                &metadata
                    .and_then(|value| value.frame_rate)
                    .map(|fps| format!("{fps:.3} fps"))
                    .unwrap_or_else(|| "—".into()),
            );
            metadata_row(
                ui,
                &t(state.language, "Audio", "音声"),
                &format_audio_metadata(metadata, state.waveforms.get(&item.id).map(Arc::as_ref)),
            );
            metadata_row(
                ui,
                &t(state.language, "Bit rate", "ビットレート"),
                &metadata
                    .and_then(|value| value.overall_bit_rate)
                    .map(format_bit_rate)
                    .unwrap_or_else(|| "—".into()),
            );
            metadata_row(
                ui,
                &t(state.language, "Size", "サイズ"),
                &metadata
                    .and_then(|value| value.file_size)
                    .map(format_file_size)
                    .unwrap_or_else(|| "—".into()),
            );
            metadata_row(
                ui,
                &t(state.language, "Container", "コンテナ"),
                metadata
                    .and_then(|value| value.container.as_deref())
                    .unwrap_or("—"),
            );
            metadata_row(
                ui,
                &t(state.language, "Monitor decoder", "モニターデコーダー"),
                state
                    .media_decoder_backends
                    .get(&item.id)
                    .map(String::as_str)
                    .unwrap_or(match state.language {
                        Language::English => "Not used yet",
                        Language::Japanese => "まだ使用されていません",
                    }),
            );
            if let Some(metadata) = metadata {
                for stream in &metadata.streams {
                    metadata_row(
                        ui,
                        &format!(
                            "{} {}",
                            t(state.language, "Stream", "ストリーム"),
                            stream.index
                        ),
                        &format_stream_metadata(stream),
                    );
                }
            }
            let status = analysis_panel_status(state.language, panel_state, false);
            metadata_row(ui, &t(state.language, "Status", "状態"), &status);
        });
        ui.add_space(5.0);
        let path = item.path.display().to_string();
        ui.add_sized(
            [ui.available_width(), 18.0],
            egui::Label::new(
                RichText::new(&path)
                    .small()
                    .color(Color32::from_rgb(116, 137, 153)),
            )
            .truncate(),
        )
        .on_hover_text(path);
    } else {
        ui.label(
            RichText::new(t(
                state.language,
                "Select imported media to inspect metadata.",
                "読み込んだメディアを選択するとメタデータを表示します。",
            ))
            .small()
            .color(Color32::from_rgb(133, 150, 165)),
        );
    }
}

fn metadata_row(ui: &mut Ui, name: &str, value: &str) {
    ui.horizontal(|ui| {
        let label_width = (ui.available_width() * 0.32).clamp(64.0, 110.0);
        ui.add_sized(
            [label_width, 18.0],
            egui::Label::new(
                RichText::new(name)
                    .small()
                    .color(Color32::from_rgb(132, 153, 169)),
            )
            .truncate(),
        )
        .on_hover_text(name);
        ui.add_sized(
            [ui.available_width(), 18.0],
            egui::Label::new(
                RichText::new(value)
                    .small()
                    .color(Color32::from_rgb(211, 221, 230)),
            )
            .truncate(),
        )
        .on_hover_text(value);
    });
}

#[cfg(test)]
/// HOT PATH — no IO. Visibility is binary-searched and drawing is O(visible clips/bands).
fn timeline(ui: &mut Ui, state: &mut EditorState, height: f32) {
    let mut canvas = EguiTimelineCanvas::default();
    timeline_with_canvas(ui, state, height, &mut canvas);
}

#[derive(Clone, Copy, Debug, Default)]
struct TimelinePresentation {
    show_tool_row: bool,
    audio_focus: Option<TrackId>,
}

fn presented_track_height(
    kind: TrackKind,
    track_id: TrackId,
    stored: f32,
    presentation: TimelinePresentation,
) -> f32 {
    match presentation.audio_focus {
        Some(_) if kind == TrackKind::Video => MIN_TIMELINE_TRACK_HEIGHT,
        Some(focus) if track_id == focus => stored.max(160.0),
        Some(_) => MIN_TIMELINE_TRACK_HEIGHT,
        None => stored,
    }
}

fn timeline_with_canvas(
    ui: &mut Ui,
    state: &mut EditorState,
    height: f32,
    canvas: &mut dyn TimelineCanvas,
) {
    timeline_with_canvas_presentation(
        ui,
        state,
        height,
        canvas,
        TimelinePresentation {
            show_tool_row: true,
            audio_focus: None,
        },
    );
}

fn timeline_with_canvas_presentation(
    ui: &mut Ui,
    state: &mut EditorState,
    height: f32,
    canvas: &mut dyn TimelineCanvas,
    presentation: TimelinePresentation,
) {
    panel_title(
        ui,
        &t(state.language, "Timeline", "タイムライン"),
        &format!(
            "{} · {}",
            t(state.language, "clips", "クリップ"),
            state
                .timeline
                .tracks
                .iter()
                .map(|track| track.clips.len())
                .sum::<usize>()
        ),
    );
    if presentation.show_tool_row {
        tool_row(ui, state);
    }
    let chrome_height = if presentation.show_tool_row {
        156.0
    } else {
        108.0
    };
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), (height - chrome_height).max(172.0)),
        Sense::click_and_drag(),
    );
    // Register the native operation before any overlay is emitted.  A native implementation
    // can retain the subsequent rect commands and execute them in this exact layer.
    canvas.begin(ui, rect);
    let painter = ui.painter();
    canvas.solid_rect(rect, Color32::from_rgb(14, 18, 24));
    let header_w = 120.0;
    let ruler_h = 24.0;
    let title_lane_h = 42.0;
    let content = Rect::from_min_max(
        Pos2::new(rect.left() + header_w, rect.top() + ruler_h + title_lane_h),
        rect.right_bottom(),
    );
    let ruler = Rect::from_min_max(
        Pos2::new(rect.left() + header_w, rect.top()),
        Pos2::new(rect.right(), rect.top() + ruler_h),
    );
    let title_lane = Rect::from_min_max(
        Pos2::new(rect.left() + header_w, ruler.bottom()),
        Pos2::new(rect.right(), ruler.bottom() + title_lane_h),
    );
    let track_viewport =
        Rect::from_min_max(Pos2::new(rect.left(), content.top()), rect.right_bottom());
    let track_height_count = state.track_heights.len();
    state
        .track_heights
        .retain(|track_id, _| state.timeline.track(*track_id).is_some());
    if state.track_heights.len() != track_height_count {
        state.mark_durable_edit();
    }
    let mut track_heights: Vec<f32> = state
        .timeline
        .tracks
        .iter()
        .map(|track| {
            let stored = state
                .track_heights
                .get(&track.id)
                .copied()
                .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
            presented_track_height(track.kind, track.id, stored, presentation)
        })
        .collect();
    let (total_track_height, max_scroll_y, mut scroll_y) =
        track_layout(&track_heights, content.height(), state.timeline_scroll_y);
    if state.timeline_scroll_y != scroll_y {
        state.timeline_scroll_y = scroll_y;
        state.mark_durable_edit();
    }
    let visible_ticks = state.timeline_view_span.0.max(1) as f32;
    let view_start = state.timeline_view_start;
    state.timeline_drop_geometry = Some(TimelineDropGeometry {
        rect,
        content,
        view_start,
        visible_ticks,
    });
    let pointer = response.interact_pointer_pos();
    let hover_pointer = ui.ctx().pointer_hover_pos();
    // `drag_started` is delivered after the pointer has moved past egui's drag threshold. Keep
    // the actual press origin so a small fade or gain control still owns that drag.
    let press_origin = ui.input(|input| input.pointer.press_origin());
    // A Media Pool drag is owned by the source widget, so the timeline response has no
    // interaction position even when it contains the released pointer. Read the active
    // pointer from the context for cross-widget drop placement.
    // The interaction position may remain owned by the Media Pool source while dragging across
    // panels. Prefer the physical latest/hover position; otherwise a real window drag can report
    // the source row here and reject a release that visibly occurred over the timeline.
    let drop_pointer = drop_pointer_position(
        ui.ctx().pointer_latest_pos(),
        hover_pointer,
        ui.ctx().pointer_interact_pos(),
    );
    // Do not route the release through the canvas response. Track resize handles and clip
    // controls are layered above it, so `Response::contains_pointer` can reject a valid drop.
    // Geometry is the authoritative drop zone; take the payload directly on release.
    let pointer_released = ui.input(|input| input.pointer.any_released());
    let released_over_timeline =
        pointer_released && drop_pointer.is_some_and(|point| rect.contains(point));
    let dropped_media = released_over_timeline
        .then(|| {
            egui::DragAndDrop::take_payload::<MediaDragPayload>(ui.ctx()).or_else(|| {
                state
                    .active_media_drag
                    .map(|media_id| Arc::new(MediaDragPayload { media_id }))
            })
        })
        .flatten();
    let dropped_transition = released_over_timeline
        .then(|| {
            egui::DragAndDrop::take_payload::<TransitionDragPayload>(ui.ctx()).or_else(|| {
                state
                    .active_transition_drag
                    .map(|kind| Arc::new(TransitionDragPayload { kind }))
            })
        })
        .flatten();
    let drop_tick = drop_pointer.and_then(|point| state.timeline_drop_start_at(point));
    if let Some(point) = hover_pointer.filter(|point| track_viewport.contains(*point))
        && state.timeline_drag.is_none()
    {
        let (scroll, shift) = ui.input(|input| (input.smooth_scroll_delta, input.modifiers.shift));
        if scroll != Vec2::ZERO {
            if point.x >= content.left() && !shift {
                let anchor = timeline_tick_at(point.x, content, view_start, visible_ticks);
                let fraction = (point.x - content.left()) / content.width().max(1.0);
                state.zoom_timeline_view(anchor, fraction, (-scroll.y * 0.0025).exp());
            } else if point.x >= content.left() {
                let pixels = if scroll.x.abs() > scroll.y.abs() {
                    scroll.x
                } else {
                    scroll.y
                };
                let delta =
                    Tick((-pixels / content.width().max(1.0) * visible_ticks).round() as i64);
                state.pan_timeline_view(delta);
            } else {
                scroll_y = (scroll_y - scroll.y).clamp(0.0, max_scroll_y);
            }
            if state.timeline_scroll_y != scroll_y {
                state.timeline_scroll_y = scroll_y;
                state.mark_durable_edit();
            }
            ui.ctx().request_repaint();
        }
    }
    canvas.solid_rect(
        Rect::from_min_size(rect.min, Vec2::new(header_w, rect.height())),
        Color32::from_rgb(20, 27, 35),
    );
    solid_line(
        canvas,
        Pos2::new(rect.left() + header_w, rect.top()),
        Pos2::new(rect.left() + header_w, rect.bottom()),
        1.0,
        Color32::from_rgb(49, 63, 75),
    );
    for i in 0..=10 {
        let x = content.left() + content.width() * i as f32 / 10.0;
        solid_line(
            canvas,
            Pos2::new(x, rect.top()),
            Pos2::new(x, rect.bottom()),
            1.0,
            Color32::from_rgb(31, 39, 48),
        );
        painter.text(
            Pos2::new(x + 4.0, rect.top() + 5.0),
            egui::Align2::LEFT_TOP,
            format!(
                "{:02}:{:02}",
                ((view_start.0 as f32 + i as f32 * visible_ticks / 10.0) / 1_000_000.0 / 60.0)
                    as u32,
                (((view_start.0 as f32 + i as f32 * visible_ticks / 10.0) / 1_000_000.0) as u32)
                    % 60
            ),
            FontId::monospace(10.0),
            Color32::from_rgb(130, 150, 165),
        );
    }
    for marker in &state.markers {
        if marker.tick < view_start
            || marker.tick.0 > view_start.0.saturating_add(visible_ticks as i64)
        {
            continue;
        }
        let x = content.left()
            + (marker.tick.0 - view_start.0) as f32 / visible_ticks * content.width();
        let color = marker_color(marker.color);
        solid_line(
            canvas,
            Pos2::new(x, ruler.top()),
            Pos2::new(x, ruler.bottom()),
            2.0,
            color,
        );
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(x - 4.0, ruler.top()),
                Pos2::new(x + 4.0, ruler.top()),
                Pos2::new(x, ruler.top() + 7.0),
            ],
            color,
            Stroke::NONE,
        ));
    }
    let playhead = content.left()
        + ((state.playhead.0 - view_start.0) as f32 / visible_ticks).clamp(0.0, 1.0)
            * content.width();
    // This stable, ruler-only interaction captures the initial pointer press before the canvas
    // sees a drag. It keeps ownership even after the pointer moves over a clip.
    let scrub_handle = playhead_handle_rect(playhead, ruler);
    let scrub_response = ui.interact(
        scrub_handle,
        ui.id().with("timeline-playhead-scrub"),
        Sense::drag(),
    );
    if scrub_response.hovered() || scrub_response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    let mut hit_clip = None;
    let mut hit_drag = None;
    let mut hit_color_keyframe = None;
    let mut press_clip = None;
    let mut press_drag = None;
    let mut press_color_keyframe = None;
    let mut press_structural_edge = None;
    let mut hit_video_transition = None;
    let mut hit_title = None;
    let mut press_title = None;
    let mut hit_resize_grip = None;
    let mut resize_drag_started = None;
    let mut resize_drag_delta = None;
    let mut resize_drag_stopped = false;
    let mut toggle_muted_track = None;
    let mut toggle_solo_track = None;
    let mut focus_audio_track = None;
    painter.rect_filled(title_lane, 0.0, Color32::from_rgb(20, 31, 43));
    painter.text(
        Pos2::new(rect.left() + 10.0, title_lane.center().y),
        egui::Align2::LEFT_CENTER,
        t(state.language, "Titles", "タイトル"),
        FontId::proportional(11.0),
        Color32::from_rgb(151, 187, 207),
    );
    for title in state.timeline.titles() {
        let title_rect = clip_rect_for(
            title_lane.shrink2(Vec2::new(2.0, 6.0)),
            title_lane,
            title.start,
            title.duration,
            view_start,
            visible_ticks,
        );
        if title_rect.width() <= 1.0 {
            continue;
        }
        let selected = state.selected_title == Some(title.id);
        painter.rect_filled(
            title_rect,
            3.0,
            if selected {
                Color32::from_rgb(75, 156, 205)
            } else {
                Color32::from_rgb(54, 103, 148)
            },
        );
        painter.rect_stroke(
            title_rect,
            3.0,
            Stroke::new(
                if selected { 2.0 } else { 1.0 },
                Color32::from_rgb(129, 210, 242),
            ),
            StrokeKind::Inside,
        );
        painter.text(
            title_rect.left_center() + Vec2::new(5.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &title.text,
            FontId::proportional(10.0),
            Color32::WHITE,
        );
        for point in [hover_pointer, press_origin].into_iter().flatten() {
            if title_lane.contains(point) && title_rect.contains(point) {
                let edge = if (point.x - title_rect.left()).abs() <= 6.0 {
                    Some(FadeEdge::In)
                } else if (point.x - title_rect.right()).abs() <= 6.0 {
                    Some(FadeEdge::Out)
                } else {
                    None
                };
                if Some(point) == press_origin {
                    press_title = Some((title.id, edge));
                } else {
                    hit_title = Some((title.id, edge));
                }
            }
        }
    }
    state.timeline_cache.rebuild_if_stale(&state.timeline);
    state.rebuild_timeline_media_draw_slots_if_stale();
    let visible_tracks =
        visible_track_range(&track_heights, content.top(), scroll_y, content.height());
    state.timeline_track_rows.clear();
    let track_painter = painter.with_clip_rect(track_viewport);
    let pixels_per_point = ui.ctx().pixels_per_point();
    if state.timeline_label_pixels_per_point != pixels_per_point {
        state.timeline_label_pixels_per_point = pixels_per_point;
        state.timeline_label_galleys.clear();
        state.timeline_offline_prefix_galley = None;
        state.timeline_waveform_pending_galley = None;
        state.timeline_waveform_failed_galley = None;
    }
    let mut row_top =
        content.top() - scroll_y + track_heights.iter().take(visible_tracks.start).sum::<f32>();
    for track_index in visible_tracks {
        let track_id = state.timeline.tracks[track_index].id;
        state
            .timeline_cache
            .track(track_id)
            .expect("timeline cache mirrors authoritative tracks")
            .write_draw_records(
                view_start,
                Tick(view_start.0.saturating_add(visible_ticks as i64)),
                content.width() as f64 / visible_ticks as f64,
                1.0,
                &mut state.timeline_draw_records,
            );
        let track = &state.timeline.tracks[track_index];
        let track_h = track_heights[track_index];
        let row = Rect::from_min_size(
            Pos2::new(content.left(), row_top),
            Vec2::new(content.width(), track_h),
        );
        row_top += track_h;
        state.timeline_track_rows.push(TimelineTrackRowGeometry {
            track_id,
            kind: track.kind,
            rect: row,
        });
        let track_is_focused = track.kind == TrackKind::Audio
            && state.undertow_track == Some(track.id)
            && state.selected_timeline_clip.is_none();
        canvas.solid_rect(
            Rect::from_min_size(
                Pos2::new(rect.left(), row.top()),
                Vec2::new(header_w, track_h),
            ),
            if track_is_focused {
                Color32::from_rgb(23, 50, 54)
            } else if track.kind == TrackKind::Video {
                Color32::from_rgb(23, 31, 40)
            } else {
                Color32::from_rgb(20, 33, 33)
            },
        );
        let ordinal = state.timeline.tracks[..=track_index]
            .iter()
            .filter(|candidate| candidate.kind == track.kind)
            .count();
        let track_label_rect = Rect::from_min_max(
            Pos2::new(rect.left() + 6.0, row.top() + 3.0),
            Pos2::new(rect.left() + 42.0, row.bottom() - 3.0),
        );
        let track_label_response = ui.interact(
            track_label_rect,
            ui.id().with(("timeline-track-label", track.id.0)),
            if track.kind == TrackKind::Audio {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        if track.kind == TrackKind::Audio && track_label_response.clicked() {
            focus_audio_track = Some(track.id);
        }
        track_painter.text(
            Pos2::new(rect.left() + 10.0, row.center().y),
            egui::Align2::LEFT_CENTER,
            match track.kind {
                TrackKind::Video => format!("V{ordinal}"),
                TrackKind::Audio => format!("A{ordinal}"),
            },
            FontId::proportional(12.0),
            if track_label_response.hovered() && track.kind == TrackKind::Audio {
                Color32::from_rgb(112, 214, 229)
            } else {
                Color32::from_rgb(184, 202, 215)
            },
        );
        let chip_h = (track_h - 6.0).clamp(14.0, 24.0);
        let mute_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 52.0, row.center().y),
            Vec2::new(22.0, chip_h),
        );
        let mute_response = ui
            .interact(
                mute_rect,
                ui.id().with(("timeline-track-mute", track.id.0)),
                Sense::click(),
            )
            .on_hover_text(t(
                state.language,
                if track.muted {
                    "Unmute track"
                } else {
                    "Mute track"
                },
                if track.muted {
                    "トラックのミュートを解除"
                } else {
                    "トラックをミュート"
                },
            ));
        paint_track_header_chip(
            &track_painter,
            mute_rect,
            "M",
            track.muted,
            mute_response.hovered(),
            Color32::from_rgb(116, 53, 57),
        );
        if mute_response.clicked() {
            toggle_muted_track = Some((track.id, !track.muted));
        }
        if track.kind == TrackKind::Audio {
            let solo_rect = Rect::from_center_size(
                Pos2::new(rect.left() + 78.0, row.center().y),
                Vec2::new(22.0, chip_h),
            );
            let solo_response = ui
                .interact(
                    solo_rect,
                    ui.id().with(("timeline-track-solo", track.id.0)),
                    Sense::click(),
                )
                .on_hover_text(t(
                    state.language,
                    if track.solo {
                        "Disable solo"
                    } else {
                        "Solo track"
                    },
                    if track.solo {
                        "ソロを解除"
                    } else {
                        "トラックをソロ"
                    },
                ));
            paint_track_header_chip(
                &track_painter,
                solo_rect,
                "S",
                track.solo,
                solo_response.hovered(),
                Color32::from_rgb(176, 132, 42),
            );
            if solo_response.clicked() {
                toggle_solo_track = Some((track.id, !track.solo));
            }
        }
        // A row boundary is the resize handle, not just the small track-label area.  Keeping
        // the inner edge below the clip inset leaves gain and fade controls unobstructed.
        let grip = track_resize_grip(row, track_viewport);
        // This must be a separate drag-only response. A click-and-drag response reports
        // `drag_started` only after the pointer moves, at which point a narrow boundary may
        // no longer be under the pointer. The stable track ID preserves the press origin.
        let grip_response = ui.interact(
            grip,
            ui.id().with(("timeline-track-resize", track.id.0)),
            Sense::drag(),
        );
        let resize_hovered = hover_pointer.is_some_and(|point| grip.contains(point));
        if grip_response.hovered() || grip_response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        solid_line(
            canvas,
            Pos2::new(rect.left(), row.bottom()),
            Pos2::new(rect.right(), row.bottom()),
            if resize_hovered { 2.0 } else { 1.5 },
            if resize_hovered {
                Color32::from_rgb(143, 199, 226)
            } else {
                Color32::from_rgb(92, 120, 138)
            },
        );
        if resize_hovered {
            hit_resize_grip = Some(track.id);
        }
        if grip_response.drag_started() {
            resize_drag_started = Some(track.id);
        }
        if let Some(delta) = grip_response.total_drag_delta() {
            resize_drag_delta = Some((track.id, delta.y));
        }
        resize_drag_stopped |= grip_response.drag_stopped();
        let mut color_keyframe_overlay = None;
        for record in state.timeline_draw_records.iter().copied() {
            let cached = match record {
                TrackDrawRecord::Clip(clip) => clip,
                TrackDrawRecord::Band(band) => {
                    let band_rect = clip_rect_for(
                        row,
                        content,
                        band.start,
                        Tick(band.end.0.saturating_sub(band.start.0).max(1)),
                        view_start,
                        visible_ticks,
                    );
                    canvas.solid_rect(
                        band_rect.shrink2(Vec2::new(0.0, 3.0)),
                        if track.kind == TrackKind::Video {
                            Color32::from_rgb(39, 104, 139)
                        } else {
                            Color32::from_rgb(42, 126, 91)
                        },
                    );
                    continue;
                }
            };
            let clip = &track.clips[cached.index];
            debug_assert_eq!(clip.id, cached.id);
            let clip_rect = clip_rect_for(
                row,
                content,
                clip.start,
                clip.duration,
                view_start,
                visible_ticks,
            );
            if clip_rect.width() <= 1.0 {
                continue;
            }
            let selected = state.selected_timeline_clip == Some(clip.id);
            let hovered = hover_pointer.is_some_and(|point| clip_rect.contains(point));
            let media_index = clip.media.0.saturating_sub(1) as usize;
            let draw_slot = state.timeline_media_draw_slots.get(media_index);
            let label_galley = if timeline_clip_label_is_visible(clip_rect) {
                state.media.get(media_index).map(|item| {
                    cached_timeline_label_galley(
                        &track_painter,
                        &mut state.timeline_label_galleys,
                        media_index,
                        &item.display_name,
                    )
                })
            } else {
                None
            };
            let offline = draw_slot.is_some_and(|slot| slot.offline);
            let offline_prefix_galley = (offline && label_galley.is_some()).then(|| {
                state
                    .timeline_offline_prefix_galley
                    .get_or_insert_with(|| {
                        track_painter.layout_no_wrap(
                            "OFFLINE · ".to_owned(),
                            FontId::proportional(10.0),
                            Color32::WHITE,
                        )
                    })
                    .clone()
            });
            let waveform = draw_slot.and_then(|slot| slot.waveform.as_deref());
            let waveform_ready = waveform.is_some_and(|waveform| !waveform.peaks.is_empty());
            let waveform_failed = draw_slot.is_some_and(|slot| slot.waveform_failed);
            let waveform_status = (track.kind == TrackKind::Audio
                && state.show_audio_waveforms
                && !waveform_ready
                && timeline_waveform_status_is_visible(clip_rect))
            .then(|| {
                if waveform_failed {
                    (
                        cached_timeline_status_galley(
                            &track_painter,
                            &mut state.timeline_waveform_failed_galley,
                            state.language,
                            "Waveform unavailable",
                            "波形を利用できません",
                        ),
                        Color32::from_rgb(213, 160, 150),
                    )
                } else {
                    (
                        cached_timeline_status_galley(
                            &track_painter,
                            &mut state.timeline_waveform_pending_galley,
                            state.language,
                            "Analyzing waveform…",
                            "波形を解析中…",
                        ),
                        Color32::from_rgb(181, 218, 199),
                    )
                }
            });
            let waveform_status_color = waveform_status
                .as_ref()
                .map_or(Color32::TRANSPARENT, |(_, color)| *color);
            let waveform_status_galley = waveform_status.map(|(galley, _)| galley);
            draw_timeline_clip(
                canvas,
                &track_painter,
                clip_rect,
                track.kind,
                clip,
                TimelineClipPaint {
                    waveform,
                    waveform_status_galley,
                    waveform_status_color,
                    offline,
                    video_strip: state
                        .show_video_thumbnails
                        .then(|| draw_slot.and_then(|slot| slot.video_strip))
                        .flatten(),
                    show_video_thumbnails: state.show_video_thumbnails,
                    show_audio_waveforms: state.show_audio_waveforms,
                    flag_color: draw_slot.and_then(|slot| slot.flag_color),
                    label_galley,
                    offline_prefix_galley,
                    selected,
                    enabled: clip.enabled,
                    show_handles: selected || hovered,
                },
            );
            if selected
                && track.kind == TrackKind::Video
                && let Some(effect_id) = active_color_effect_for_clip(state, clip)
                && let Some(node) = clip.video_effects.iter().find(|node| node.id == effect_id)
            {
                color_keyframe_overlay = Some((clip_rect, clip.id, effect_id));
                if let Some(point) = hover_pointer.filter(|point| track_viewport.contains(*point)) {
                    hit_color_keyframe = color_keyframe_hit(
                        clip_rect,
                        content,
                        view_start,
                        visible_ticks,
                        clip,
                        node,
                        point,
                    );
                }
                if let Some(point) = press_origin.filter(|point| track_viewport.contains(*point)) {
                    press_color_keyframe = color_keyframe_hit(
                        clip_rect,
                        content,
                        view_start,
                        visible_ticks,
                        clip,
                        node,
                        point,
                    );
                }
            }
            if let Some(point) = hover_pointer.filter(|point| track_viewport.contains(*point))
                && let Some(drag) = clip_hit_at_pointer(
                    clip_rect,
                    track.kind,
                    clip,
                    point,
                    visible_ticks,
                    content.width(),
                )
            {
                hit_clip = Some((track.id, clip.id));
                hit_drag = drag;
            }
            if let Some(point) = press_origin.filter(|point| track_viewport.contains(*point))
                && let Some(drag) = clip_hit_at_pointer(
                    clip_rect,
                    track.kind,
                    clip,
                    point,
                    visible_ticks,
                    content.width(),
                )
            {
                press_clip = Some((track.id, clip.id));
                press_drag = drag;
                press_structural_edge = clip_structural_edge_hit(clip_rect, point);
            }
        }
        if track.kind == TrackKind::Video {
            for transition in state
                .timeline
                .transitions()
                .iter()
                .filter(|transition| transition.track_id == track.id)
            {
                let Some((start, end)) = state.timeline.transition_timing(transition.id) else {
                    continue;
                };
                let transition_rect = clip_rect_for(
                    row.shrink2(Vec2::new(0.0, 6.0)),
                    content,
                    start,
                    Tick(end.0.saturating_sub(start.0)),
                    view_start,
                    visible_ticks,
                );
                if transition_rect.width() <= 1.0 {
                    continue;
                }
                let selected = [transition.left_clip, transition.right_clip]
                    .contains(&state.selected_timeline_clip.unwrap_or(ClipId(0)));
                let (fill, stroke_color) = transition_timeline_colors(transition.kind, selected);
                track_painter.rect_filled(transition_rect, 2.0, fill);
                track_painter.rect_stroke(
                    transition_rect,
                    2.0,
                    Stroke::new(if selected { 2.0 } else { 1.0 }, stroke_color),
                    StrokeKind::Inside,
                );
                match transition.kind {
                    VideoTransitionKind::CrossDissolve | VideoTransitionKind::FilmDissolve => {
                        track_painter.line_segment(
                            [transition_rect.left_top(), transition_rect.right_bottom()],
                            Stroke::new(1.0, Color32::from_rgb(211, 240, 250)),
                        );
                        track_painter.line_segment(
                            [transition_rect.left_bottom(), transition_rect.right_top()],
                            Stroke::new(1.0, Color32::from_rgb(211, 240, 250)),
                        );
                    }
                    VideoTransitionKind::DipToBlack | VideoTransitionKind::DipToWhite => {
                        let center_bottom =
                            Pos2::new(transition_rect.center().x, transition_rect.bottom() - 1.0);
                        let stroke = Stroke::new(1.2, Color32::from_rgb(224, 229, 234));
                        track_painter
                            .line_segment([transition_rect.left_top(), center_bottom], stroke);
                        track_painter
                            .line_segment([center_bottom, transition_rect.right_top()], stroke);
                    }
                    VideoTransitionKind::WipeLeft | VideoTransitionKind::WipeRight => {
                        let (from, to) = if transition.kind == VideoTransitionKind::WipeLeft {
                            (
                                transition_rect.left_center(),
                                transition_rect.right_center(),
                            )
                        } else {
                            (
                                transition_rect.right_center(),
                                transition_rect.left_center(),
                            )
                        };
                        track_painter.line_segment([from, to], Stroke::new(1.4, Color32::WHITE));
                    }
                    VideoTransitionKind::WipeUp | VideoTransitionKind::WipeDown => {
                        let (from, to) = if transition.kind == VideoTransitionKind::WipeUp {
                            (
                                Pos2::new(transition_rect.center().x, transition_rect.bottom()),
                                transition_rect.center_top(),
                            )
                        } else {
                            (
                                transition_rect.center_top(),
                                transition_rect.center_bottom(),
                            )
                        };
                        track_painter.line_segment([from, to], Stroke::new(1.4, Color32::WHITE));
                    }
                    VideoTransitionKind::SlideFromLeft
                    | VideoTransitionKind::SlideFromRight
                    | VideoTransitionKind::SlideFromTop
                    | VideoTransitionKind::SlideFromBottom => {
                        track_painter.rect_stroke(
                            transition_rect.shrink(4.0),
                            1.0,
                            Stroke::new(1.2, Color32::WHITE),
                            StrokeKind::Inside,
                        );
                    }
                }
                if hover_pointer.is_some_and(|point| transition_rect.contains(point)) {
                    hit_video_transition = Some((transition.left_clip, transition.right_clip));
                }
            }
        } else {
            for transition in state
                .timeline
                .audio_transitions()
                .iter()
                .filter(|transition| transition.track_id == track.id)
            {
                let Some((start, end)) = state.timeline.audio_transition_timing(transition.id)
                else {
                    continue;
                };
                let transition_rect = clip_rect_for(
                    row.shrink2(Vec2::new(0.0, 6.0)),
                    content,
                    start,
                    Tick(end.0.saturating_sub(start.0)),
                    view_start,
                    visible_ticks,
                );
                if transition_rect.width() <= 1.0 {
                    continue;
                }
                let selected = [transition.left_clip, transition.right_clip]
                    .contains(&state.selected_timeline_clip.unwrap_or(ClipId(0)));
                track_painter.rect_filled(
                    transition_rect,
                    2.0,
                    if selected {
                        Color32::from_rgba_unmultiplied(213, 146, 62, 210)
                    } else {
                        Color32::from_rgba_unmultiplied(173, 111, 47, 178)
                    },
                );
                track_painter.rect_stroke(
                    transition_rect,
                    2.0,
                    Stroke::new(
                        if selected { 2.0 } else { 1.0 },
                        Color32::from_rgb(255, 220, 150),
                    ),
                    StrokeKind::Inside,
                );
                let center = transition_rect.center().y;
                let stroke = Stroke::new(1.2, Color32::from_rgb(255, 239, 194));
                track_painter.line_segment(
                    [
                        transition_rect.left_bottom(),
                        Pos2::new(transition_rect.right(), center),
                    ],
                    stroke,
                );
                track_painter.line_segment(
                    [
                        Pos2::new(transition_rect.left(), center),
                        transition_rect.right_top(),
                    ],
                    stroke,
                );
            }
        }
        if let Some((clip_rect, clip_id, effect_id)) = color_keyframe_overlay
            && let Some(clip) = state.timeline.clip(clip_id)
            && let Some(node) = clip.video_effects.iter().find(|node| node.id == effect_id)
        {
            // Paint after transitions so every visible key also has a visible hit target.
            draw_timeline_color_keyframes(
                &track_painter,
                clip_rect,
                content,
                view_start,
                visible_ticks,
                clip,
                node,
            );
        }
    }
    if let Some((start, end)) = state.range_selection {
        let left_tick = start.min(end);
        let right_tick = start.max(end);
        let left =
            content.left() + (left_tick.0 - view_start.0) as f32 / visible_ticks * content.width();
        let right =
            content.left() + (right_tick.0 - view_start.0) as f32 / visible_ticks * content.width();
        let selection = Rect::from_min_max(
            Pos2::new(left.clamp(content.left(), content.right()), content.top()),
            Pos2::new(
                right.clamp(content.left(), content.right()),
                content.bottom(),
            ),
        );
        if selection.width() > 0.0 {
            painter.rect_filled(
                selection,
                0.0,
                Color32::from_rgba_unmultiplied(57, 157, 207, 42),
            );
            painter.rect_stroke(
                selection,
                0.0,
                Stroke::new(1.0, Color32::from_rgb(93, 190, 232)),
                StrokeKind::Inside,
            );
        }
    }
    if let Some((track_id, muted)) = toggle_muted_track {
        let before = state.timeline_history_checkpoint();
        let generation = state.timeline.generation();
        if state.timeline.set_track_muted(track_id, muted).is_ok()
            && state.mark_changed_timeline_generation(generation)
        {
            state.record_timeline_history(before);
        }
    }
    if let Some((track_id, solo)) = toggle_solo_track {
        let before = state.timeline_history_checkpoint();
        let generation = state.timeline.generation();
        if state.timeline.set_track_solo(track_id, solo).is_ok()
            && state.mark_changed_timeline_generation(generation)
        {
            state.record_timeline_history(before);
        }
    }
    if let Some(track_id) = focus_audio_track {
        focus_audio_track_in_audio_tab(state, track_id);
    }
    // Thumbnail tiles are still emitted by egui after the native base callback. Keep the
    // playhead in this overlay layer so video imagery can never cover it.
    painter.line_segment(
        [
            Pos2::new(playhead, rect.top()),
            Pos2::new(playhead, rect.bottom()),
        ],
        Stroke::new(1.5, Color32::from_rgb(239, 77, 81)),
    );
    draw_playhead_handle(canvas, painter, scrub_handle);
    draw_track_scrollbar(canvas, content, total_track_height, scroll_y);
    if state
        .timeline
        .tracks
        .iter()
        .all(|track| track.clips.is_empty())
        && state.timeline.titles().is_empty()
    {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            t(
                state.language,
                "Drop media here to begin editing",
                "メディアをここにドロップして編集を開始",
            ),
            FontId::proportional(14.0),
            Color32::from_rgb(126, 147, 164),
        );
    }
    if response.clicked_by(egui::PointerButton::Primary) {
        if let Some(keyframe) = press_color_keyframe.or(hit_color_keyframe) {
            state.selected_timeline_clip = Some(keyframe.clip_id);
            state.selected_title = None;
            state.active_color_effect = Some(keyframe.effect_id);
            if let Some(clip) = state.timeline.clip(keyframe.clip_id)
                && let Some(tick) = color_keyframe_timeline_tick(clip, keyframe.source_tick)
            {
                state.set_playhead(tick);
            }
        } else if press_drag.is_none()
            && let Some((left_clip, _)) = hit_video_transition
        {
            state.selected_timeline_clip = Some(left_clip);
            state.selected_title = None;
            state.right_sidebar_tab = RightSidebarTab::Effects;
            state.mark_durable_edit();
        } else if let Some((title_id, _)) =
            hit_title.filter(|_| pointer.is_some_and(|point| title_lane.contains(point)))
        {
            if state.selected_title != Some(title_id) {
                state.selected_title = Some(title_id);
                state.selected_timeline_clip = None;
                state.mark_durable_edit();
            }
        } else if state.tool == TimelineTool::Range {
            if let Some(point) = pointer.filter(|point| content.contains(*point)) {
                let tick = timeline_tick_at(point.x, content, view_start, visible_ticks);
                state.range_selection = Some((tick, tick));
            }
        } else if state.tool == TimelineTool::Razor {
            if let Some(point) = pointer
                && content.contains(point)
            {
                let track_index = track_row_at_y(
                    &track_heights,
                    content.top(),
                    state.timeline_scroll_y,
                    point.y,
                );
                let at = timeline_tick_at(point.x, content, view_start, visible_ticks);
                let before = state.timeline_history_checkpoint();
                if let Some(track_id) = track_index
                    .and_then(|index| state.timeline.tracks.get(index))
                    .map(|track| track.id)
                    && let Ok(splits) = if state.linked_selection {
                        state.timeline.razor_linked(track_id, at)
                    } else {
                        state
                            .timeline
                            .razor(track_id, at)
                            .map(|split| split.into_iter().collect())
                    }
                    && let Some(split) = splits.first()
                {
                    state.abandon_provisional_timing(
                        splits.iter().flat_map(|split| [split.left, split.right]),
                    );
                    state.record_timeline_history(before);
                    state.selected_timeline_clip = Some(split.right);
                    state.selected_title = None;
                    state.mark_durable_edit();
                }
            }
        } else if let Some(point) = pointer
            && content.contains(point)
        {
            state.set_playhead(timeline_tick_at(
                point.x,
                content,
                view_start,
                visible_ticks,
            ));
            if let Some((_, clip)) = hit_clip {
                state.selected_timeline_clip = Some(clip);
                state.selected_title = None;
                state.mark_durable_edit();
            }
        }
    }
    if scrub_response.drag_started() {
        state.playing = false;
        state.timeline_drag = Some(TimelineDrag::Scrub);
        if let Some(point) = scrub_response.interact_pointer_pos() {
            state.set_playhead(timeline_tick_at(
                point.x,
                content,
                view_start,
                visible_ticks,
            ));
            state.clear_stale_monitor_for_scrub_gap();
        }
    } else if ui.input(|input| input.pointer.primary_pressed())
        && let Some(keyframe) = press_color_keyframe
    {
        state.selected_timeline_clip = Some(keyframe.clip_id);
        state.selected_title = None;
        state.active_color_effect = Some(keyframe.effect_id);
        if let Some(clip) = state.timeline.clip(keyframe.clip_id)
            && let Some(tick) = color_keyframe_timeline_tick(clip, keyframe.source_tick)
        {
            state.set_playhead(tick);
        }
        state.timeline_drag = Some(TimelineDrag::ColorKeyframe(keyframe));
        state.begin_timeline_history();
    } else if ui.input(|input| input.pointer.primary_pressed())
        && let Some((title_id, edge)) = press_title
    {
        if state.selected_title != Some(title_id) {
            state.selected_title = Some(title_id);
            state.selected_timeline_clip = None;
            state.mark_durable_edit();
        }
        state.timeline_drag = if let Some(edge) = edge {
            Some(TimelineDrag::TitleTrim { title_id, edge })
        } else {
            let grab_offset = press_origin
                .map(|point| timeline_tick_at(point.x, content, view_start, visible_ticks))
                .and_then(|tick| {
                    state
                        .timeline
                        .title(title_id)
                        .map(|title| Tick(tick.0 - title.start.0))
                })
                .unwrap_or(Tick(0));
            Some(TimelineDrag::TitleMove {
                title_id,
                grab_offset,
            })
        };
        state.begin_timeline_history();
    } else if ui.input(|input| input.pointer.primary_pressed())
        && let Some(handle) = press_drag
    {
        // Clip controls must claim the physical press directly. Depending on the native canvas
        // response made the gain line wait for egui's drag threshold and could let another
        // overlapping response own the gesture before the line ever received an update.
        state.timeline_drag = Some(handle);
        state.begin_timeline_history();
    } else if state.timeline_drag.is_none()
        && let (Some(handle), Some(_)) = (press_drag, resize_drag_started)
    {
        // A neighboring track's deliberately generous resize grip can overlap the top of this
        // clip. The visible fade/gain handle under the press still owns the gesture.
        state.timeline_drag = Some(handle);
        state.begin_timeline_history();
    } else if state.timeline_drag.is_none()
        && let Some(track_id) = resize_drag_started
    {
        let start_height = state
            .track_heights
            .get(&track_id)
            .copied()
            .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
        state.timeline_drag = Some(TimelineDrag::ResizeTrack {
            track_id,
            start_height,
        });
    } else if state.timeline_drag.is_none() && response.drag_started() {
        let pan_started = response.drag_started_by(egui::PointerButton::Middle)
            || (response.drag_started_by(egui::PointerButton::Primary)
                && ui.input(|input| input.modifiers.alt));
        if pan_started {
            state.timeline_drag = Some(TimelineDrag::Pan);
        } else if state.tool == TimelineTool::Range {
            let anchor = press_origin
                .or(pointer)
                .map(|point| timeline_tick_at(point.x, content, view_start, visible_ticks))
                .unwrap_or(view_start);
            state.range_selection = Some((anchor, anchor));
            state.timeline_drag = Some(TimelineDrag::Range { anchor });
        } else if let Some(track_id) = hit_resize_grip {
            let start_height = state
                .track_heights
                .get(&track_id)
                .copied()
                .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
            state.timeline_drag = Some(TimelineDrag::ResizeTrack {
                track_id,
                start_height,
            });
        } else if let Some((_, clip)) = press_clip.or(hit_clip) {
            let target_from_press = press_clip.is_some();
            let initial_drag = if target_from_press {
                press_drag
            } else {
                hit_drag
            };
            if state.selected_timeline_clip != Some(clip) {
                state.selected_timeline_clip = Some(clip);
                state.selected_title = None;
                state.mark_durable_edit();
            }
            // A visible fade or gain control owns its pointer gesture, even when a zoomed-out
            // clip puts that control inside the structural edge hit slop. Otherwise dragging a
            // fade curve point can unexpectedly trim the clip instead.
            state.timeline_drag = if let Some(handle) = initial_drag {
                Some(handle)
            } else if let Some(edge) = press_structural_edge {
                let last_tick = press_origin
                    .or(pointer)
                    .map(|point| timeline_tick_at(point.x, content, view_start, visible_ticks))
                    .unwrap_or(Tick(0));
                if state.position_lock {
                    Some(TimelineDrag::Trim {
                        clip_id: clip,
                        edge,
                        last_tick,
                        ripple: false,
                    })
                } else if state.tool == TimelineTool::Trim
                    && let Some((left_clip, right_clip)) =
                        timeline_roll_pair(&state.timeline, clip, edge)
                {
                    Some(TimelineDrag::Roll {
                        left_clip,
                        right_clip,
                        last_tick,
                    })
                } else {
                    Some(TimelineDrag::Trim {
                        clip_id: clip,
                        edge,
                        last_tick,
                        ripple: state.tool == TimelineTool::Trim,
                    })
                }
            } else if matches!(state.tool, TimelineTool::Trim | TimelineTool::Slip) {
                let last_tick = press_origin
                    .or(pointer)
                    .map(|point| timeline_tick_at(point.x, content, view_start, visible_ticks))
                    .unwrap_or(Tick(0));
                Some(TimelineDrag::Slip {
                    clip_id: clip,
                    last_tick,
                })
            } else if state.tool == TimelineTool::Pointer {
                let drag_origin = if target_from_press {
                    press_origin.or(pointer)
                } else {
                    pointer
                };
                let grab_offset = drag_origin
                    .map(|point| timeline_tick_at(point.x, content, view_start, visible_ticks))
                    .and_then(|tick| {
                        state
                            .timeline
                            .clip(clip)
                            .map(|clip| Tick(tick.0 - clip.start.0))
                    })
                    .unwrap_or(Tick(0));
                Some(TimelineDrag::Move {
                    clip_id: clip,
                    grab_offset,
                })
            } else {
                None
            };
        }
        if matches!(
            state.timeline_drag,
            Some(
                TimelineDrag::Move { .. }
                    | TimelineDrag::Gain(_)
                    | TimelineDrag::FadeDuration(_, _)
                    | TimelineDrag::FadeCurve(_, _)
                    | TimelineDrag::ColorKeyframe(_)
                    | TimelineDrag::Trim { .. }
                    | TimelineDrag::Roll { .. }
                    | TimelineDrag::Slip { .. }
            )
        ) {
            state.begin_timeline_history();
        }
    }
    if let (
        Some((resized_track_id, drag_y)),
        Some(TimelineDrag::ResizeTrack {
            track_id,
            start_height,
        }),
    ) = (resize_drag_delta, state.timeline_drag)
        && resized_track_id == track_id
    {
        let height = clamp_track_height(start_height + drag_y);
        if state.track_heights.insert(track_id, height) != Some(height) {
            state.mark_durable_edit();
        }
        if let Some(index) = state
            .timeline
            .tracks
            .iter()
            .position(|track| track.id == track_id)
        {
            track_heights[index] = height;
        }
        let (_, max_scroll, clamped_scroll) =
            track_layout(&track_heights, content.height(), state.timeline_scroll_y);
        let next_scroll = clamped_scroll.min(max_scroll);
        if state.timeline_scroll_y != next_scroll {
            state.timeline_scroll_y = next_scroll;
            state.mark_durable_edit();
        }
    }
    let clip_control_drag = matches!(
        state.timeline_drag,
        Some(
            TimelineDrag::Gain(_)
                | TimelineDrag::FadeDuration(_, _)
                | TimelineDrag::FadeCurve(_, _)
        )
    );
    let dragging =
        response.dragged() || (clip_control_drag && ui.input(|input| input.pointer.primary_down()));
    let drag_pointer = if clip_control_drag {
        ui.ctx().pointer_latest_pos().or(pointer)
    } else {
        pointer
    };
    if let Some(TimelineDrag::Scrub) = state.timeline_drag {
        if let Some(pointer) = scrub_response.interact_pointer_pos()
            && scrub_response.dragged()
        {
            state.set_playhead(timeline_tick_at(
                pointer.x,
                content,
                view_start,
                visible_ticks,
            ));
            state.clear_stale_monitor_for_scrub_gap();
            // Publish every drag update promptly; the decoder coalesces superseded targets and
            // the viewer holds the newest completed live frame until its replacement arrives.
            ui.ctx().request_repaint();
        }
    } else if dragging && let (Some(pointer), Some(drag)) = (drag_pointer, state.timeline_drag) {
        match drag {
            TimelineDrag::Scrub => unreachable!("scrub is owned by the ruler handle"),
            TimelineDrag::Move {
                clip_id,
                grab_offset,
            } => {
                if !state.position_lock
                    && let Some(clip) = state.timeline.clip(clip_id)
                {
                    let desired = timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                    let target_start = snap_move_start(
                        state,
                        Tick((desired.0 - grab_offset.0).max(0)),
                        clip.duration,
                        visible_ticks,
                        content.width(),
                        clip_id,
                    );
                    let delta = Tick(target_start.0 - clip.start.0);
                    if delta.0 != 0
                        && state
                            .timeline
                            .move_clip_with_link(clip_id, delta, state.linked_selection)
                            .is_ok()
                    {
                        state.mark_durable_edit();
                    }
                }
            }
            TimelineDrag::TitleMove {
                title_id,
                grab_offset,
            } => {
                if !state.position_lock
                    && let Some(title) = state.timeline.title(title_id).cloned()
                {
                    let desired = timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                    let mut next = title.clone();
                    next.start = Tick((desired.0 - grab_offset.0).max(0));
                    if next != title && state.timeline.replace_title(title_id, next).is_ok() {
                        state.mark_durable_edit();
                    }
                }
            }
            TimelineDrag::TitleTrim { title_id, edge } => {
                if !state.position_lock
                    && let Some(title) = state.timeline.title(title_id).cloned()
                {
                    let tick = timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                    let end = title.start.0.saturating_add(title.duration.0);
                    let mut next = title.clone();
                    match edge {
                        FadeEdge::In => {
                            let start = tick.0.clamp(0, end.saturating_sub(1));
                            next.duration = Tick(end - start);
                            next.start = Tick(start);
                        }
                        FadeEdge::Out => next.duration = Tick((tick.0 - title.start.0).max(1)),
                    }
                    if next != title && state.timeline.replace_title(title_id, next).is_ok() {
                        state.mark_durable_edit();
                    }
                }
            }
            TimelineDrag::Gain(clip_id) => {
                if let Some(clip) = state.timeline.clip(clip_id) {
                    let row_index = state
                        .timeline
                        .tracks
                        .iter()
                        .position(|track| track.id == clip.track_id)
                        .unwrap_or(0);
                    let row_height = track_heights
                        .get(row_index)
                        .copied()
                        .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
                    let row_center = content.top()
                        + track_heights.iter().take(row_index).sum::<f32>()
                        - state.timeline_scroll_y
                        + row_height * 0.5;
                    let row_rect = Rect::from_center_size(
                        Pos2::new(content.center().x, row_center),
                        Vec2::new(content.width(), row_height),
                    );
                    let clip_rect = clip_rect_for(
                        row_rect,
                        content,
                        clip.start,
                        clip.duration,
                        view_start,
                        visible_ticks,
                    );
                    let gain = audio_gain_at_y(clip_rect, pointer.y);
                    let generation = state.timeline.generation();
                    let _ = state.timeline.set_audio_gain(clip_id, gain);
                    state.mark_changed_timeline_generation(generation);
                    // Gain is a live mixer control. Keep the line/readout and the app-level
                    // audio transport updating for every pointer sample during the gesture.
                    ui.ctx().request_repaint();
                }
            }
            TimelineDrag::FadeDuration(clip_id, edge) => {
                if let Some(clip) = state.timeline.clip(clip_id) {
                    let tick = timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                    let duration = match edge {
                        FadeEdge::In => Tick((tick.0 - clip.start.0).max(0)),
                        FadeEdge::Out => Tick((clip.end().0 - tick.0).max(0)),
                    };
                    let generation = state.timeline.generation();
                    let _ = state.timeline.set_fade_duration(clip_id, edge, duration);
                    state.mark_changed_timeline_generation(generation);
                }
            }
            TimelineDrag::FadeCurve(clip_id, edge) => {
                if let Some(clip) = state.timeline.clip(clip_id) {
                    let row_index = state
                        .timeline
                        .tracks
                        .iter()
                        .position(|track| track.id == clip.track_id)
                        .unwrap_or(0);
                    let row_height = track_heights
                        .get(row_index)
                        .copied()
                        .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
                    let row_top = content.top() + track_heights.iter().take(row_index).sum::<f32>()
                        - state.timeline_scroll_y;
                    let row_rect = Rect::from_min_size(
                        Pos2::new(content.left(), row_top),
                        Vec2::new(content.width(), row_height),
                    );
                    let clip_rect = clip_rect_for(
                        row_rect,
                        content,
                        clip.start,
                        clip.duration,
                        view_start,
                        visible_ticks,
                    );
                    let curve = fade_curve_at_y(clip_rect, pointer.y);
                    let generation = state.timeline.generation();
                    let _ = state.timeline.set_fade_curve(clip_id, edge, curve);
                    state.mark_changed_timeline_generation(generation);
                }
            }
            TimelineDrag::ColorKeyframe(keyframe) => {
                if let Some(clip) = state.timeline.clip(keyframe.clip_id).cloned() {
                    let pointer_tick =
                        timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                    let requested = state.quantize_tick_to_frame_start(Tick(
                        pointer_tick.0.saturating_sub(keyframe.grab_offset.0),
                    ));
                    let timeline_tick = Tick(requested.0.clamp(clip.start.0, clip.end().0));
                    let target_source_tick = Tick(
                        clip.source_in
                            .0
                            .saturating_add(timeline_tick.0.saturating_sub(clip.start.0)),
                    );
                    let generation = state.timeline.generation();
                    if retime_color_keyframe(&mut state.timeline, keyframe, target_source_tick) {
                        state.mark_changed_timeline_generation(generation);
                        state.timeline_drag =
                            Some(TimelineDrag::ColorKeyframe(TimelineColorKeyframe {
                                source_tick: target_source_tick,
                                ..keyframe
                            }));
                        state.set_playhead_inner(timeline_tick, false);
                        ui.ctx().request_repaint();
                    }
                }
            }
            TimelineDrag::Trim {
                clip_id,
                edge,
                last_tick,
                ripple,
            } => {
                let requested = snap_timeline_tick(
                    state,
                    timeline_tick_at(pointer.x, content, view_start, visible_ticks),
                    visible_ticks,
                    content.width(),
                    Some(clip_id),
                );
                let delta = Tick(requested.0 - last_tick.0);
                let ripple = ripple && !state.position_lock;
                let affected = state
                    .replace_affected_clips(clip_id)
                    .into_iter()
                    .map(|clip| clip.id)
                    .collect::<Vec<_>>();
                let generation = state.timeline.generation();
                let result = match edge {
                    FadeEdge::In => {
                        state
                            .timeline
                            .trim_start(clip_id, delta, state.linked_selection, ripple)
                    }
                    FadeEdge::Out => {
                        state
                            .timeline
                            .trim_end(clip_id, delta, state.linked_selection, ripple)
                    }
                };
                if result.is_ok() {
                    if state.mark_changed_timeline_generation(generation) {
                        state.abandon_provisional_timing(affected);
                    }
                    state.timeline_drag = Some(TimelineDrag::Trim {
                        clip_id,
                        edge,
                        last_tick: requested,
                        ripple,
                    });
                }
            }
            TimelineDrag::Roll {
                left_clip,
                right_clip,
                last_tick,
            } => {
                let requested = snap_timeline_tick(
                    state,
                    timeline_tick_at(pointer.x, content, view_start, visible_ticks),
                    visible_ticks,
                    content.width(),
                    Some(left_clip),
                );
                let delta = Tick(requested.0 - last_tick.0);
                let affected = state
                    .replace_affected_clips(left_clip)
                    .into_iter()
                    .chain(state.replace_affected_clips(right_clip))
                    .map(|clip| clip.id)
                    .collect::<Vec<_>>();
                let generation = state.timeline.generation();
                if !state.position_lock
                    && state
                        .timeline
                        .roll_edit(left_clip, right_clip, delta, state.linked_selection)
                        .is_ok()
                {
                    if state.mark_changed_timeline_generation(generation) {
                        state.abandon_provisional_timing(affected);
                    }
                    state.timeline_drag = Some(TimelineDrag::Roll {
                        left_clip,
                        right_clip,
                        last_tick: requested,
                    });
                }
            }
            TimelineDrag::Slip { clip_id, last_tick } => {
                let requested = timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                let delta = Tick(requested.0 - last_tick.0);
                let affected = state
                    .replace_affected_clips(clip_id)
                    .into_iter()
                    .map(|clip| clip.id)
                    .collect::<Vec<_>>();
                let generation = state.timeline.generation();
                if state
                    .timeline
                    .slip_clip(clip_id, delta, state.linked_selection)
                    .is_ok()
                {
                    if state.mark_changed_timeline_generation(generation) {
                        state.abandon_provisional_timing(affected);
                    }
                    state.timeline_drag = Some(TimelineDrag::Slip {
                        clip_id,
                        last_tick: requested,
                    });
                }
            }
            TimelineDrag::Range { anchor } => {
                let current = timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                state.range_selection = Some((anchor, current));
                ui.ctx().request_repaint();
            }
            TimelineDrag::Pan => {
                let delta_x = ui.input(|input| input.pointer.delta().x);
                if delta_x != 0.0 {
                    state.pan_timeline_view(Tick(
                        (-delta_x / content.width().max(1.0) * visible_ticks).round() as i64,
                    ));
                    ui.ctx().request_repaint();
                }
            }
            TimelineDrag::ResizeTrack { .. } => {}
        }
    }
    if response.dragged()
        && state.timeline_drag.is_none()
        && state.tool != TimelineTool::Razor
        && let Some(point) = pointer
        && content.contains(point)
    {
        state.set_playhead(timeline_tick_at(
            point.x,
            content,
            view_start,
            visible_ticks,
        ));
    }
    let color_keyframe_released =
        matches!(state.timeline_drag, Some(TimelineDrag::ColorKeyframe(_)))
            && ui.input(|input| input.pointer.button_released(egui::PointerButton::Primary));
    if response.drag_stopped()
        || resize_drag_stopped
        || scrub_response.drag_stopped()
        || color_keyframe_released
    {
        state.commit_timeline_history();
        state.timeline_drag = None;
    }
    let internal_media_drag = state.active_media_drag.is_some()
        || egui::DragAndDrop::has_payload_of_type::<MediaDragPayload>(ui.ctx());
    let internal_transition_drag = state.active_transition_drag.is_some()
        || egui::DragAndDrop::has_payload_of_type::<TransitionDragPayload>(ui.ctx());
    let transition_drop_target = drop_pointer.and_then(|point| {
        dropped_transition
            .as_ref()
            .map(|payload| (payload.kind, point))
            .or_else(|| state.active_transition_drag.map(|kind| (kind, point)))
            .and_then(|(kind, point)| state.transition_drop_target_at(point, kind))
    });
    if drop_tick.is_some() && (state.drop_hovered || internal_media_drag) {
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(21, 105, 146, 42));
        painter.rect_stroke(
            rect.shrink(2.0),
            0.0,
            Stroke::new(2.0, Color32::from_rgb(101, 202, 239)),
            StrokeKind::Inside,
        );
    }
    if let Some((_, left, _)) = transition_drop_target {
        let cut = state.timeline.clip(left).map(Clip::end).unwrap_or(Tick(0));
        let x = content.left()
            + (cut.0 - view_start.0) as f32 / visible_ticks.max(1.0) * content.width();
        painter.line_segment(
            [Pos2::new(x, content.top()), Pos2::new(x, content.bottom())],
            Stroke::new(3.0, Color32::from_rgb(105, 218, 245)),
        );
    }
    if internal_transition_drag {
        let (message, color) = if transition_drop_target.is_some() {
            (
                t(
                    state.language,
                    "Release to apply transition",
                    "離してトランジションを適用",
                ),
                Color32::from_rgb(105, 218, 245),
            )
        } else {
            (
                t(
                    state.language,
                    "Drop on an unused cut between adjacent video clips",
                    "隣接するビデオクリップ間の未使用カットにドロップ",
                ),
                Color32::from_rgb(239, 143, 114),
            )
        };
        if let Some(point) = drop_pointer.filter(|point| rect.contains(*point)) {
            let label = painter.layout_no_wrap(
                message.to_owned(),
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            let feedback_rect = Rect::from_min_size(
                point + Vec2::new(14.0, 14.0),
                label.size() + Vec2::new(14.0, 8.0),
            );
            painter.rect_filled(feedback_rect, 4.0, Color32::from_rgb(20, 29, 37));
            painter.rect_stroke(
                feedback_rect,
                4.0,
                Stroke::new(1.0, color),
                StrokeKind::Inside,
            );
            painter.galley(
                feedback_rect.center() - label.size() * 0.5,
                label,
                Color32::WHITE,
            );
        }
    }
    if pointer_released
        && let (Some((track_id, left, right)), Some(kind)) = (
            transition_drop_target,
            dropped_transition
                .as_ref()
                .map(|payload| payload.kind)
                .or(state.active_transition_drag),
        )
    {
        // Native drag ownership can outlive egui's payload. A retained payload gets the same
        // validated, single-history-step operation as an egui-owned drop.
        let _ = state.add_video_transition_at_cut(track_id, left, right, kind);
    }
    if let (Some(payload), Some(start)) = (dropped_media, drop_tick) {
        state.overwrite_media_at(payload.media_id, start);
    }
    if pointer_released {
        state.active_media_drag = None;
        state.active_transition_drag = None;
    }
    let gain_feedback_clip = match state.timeline_drag {
        Some(TimelineDrag::Gain(clip_id)) => Some(clip_id),
        _ => match hit_drag {
            Some(TimelineDrag::Gain(clip_id)) => Some(clip_id),
            _ => None,
        },
    };
    if let Some(clip) = gain_feedback_clip.and_then(|clip_id| state.timeline.clip(clip_id))
        && let Some(point) = pointer.or(hover_pointer)
    {
        draw_gain_readout(painter, rect, point, clip.gain_db);
    }
    if scrub_response.hovered() || scrub_response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    } else if response.hovered() {
        if hit_color_keyframe.is_some()
            || matches!(state.timeline_drag, Some(TimelineDrag::ColorKeyframe(_)))
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        } else {
            ui.ctx().set_cursor_icon(match hit_drag {
                Some(TimelineDrag::Scrub) => egui::CursorIcon::ResizeHorizontal,
                Some(TimelineDrag::Move { .. } | TimelineDrag::TitleMove { .. }) => {
                    egui::CursorIcon::Grab
                }
                Some(TimelineDrag::ResizeTrack { .. }) => egui::CursorIcon::ResizeVertical,
                Some(TimelineDrag::Gain(_) | TimelineDrag::FadeCurve(_, _)) => {
                    egui::CursorIcon::ResizeVertical
                }
                Some(TimelineDrag::FadeDuration(_, _) | TimelineDrag::ColorKeyframe(_)) => {
                    egui::CursorIcon::ResizeHorizontal
                }
                Some(
                    TimelineDrag::Trim { .. }
                    | TimelineDrag::TitleTrim { .. }
                    | TimelineDrag::Roll { .. }
                    | TimelineDrag::Slip { .. },
                ) => egui::CursorIcon::ResizeHorizontal,
                Some(TimelineDrag::Range { .. }) => egui::CursorIcon::Crosshair,
                Some(TimelineDrag::Pan) => egui::CursorIcon::Grabbing,
                None if hit_resize_grip.is_some() => egui::CursorIcon::ResizeVertical,
                None if state.tool == TimelineTool::Razor => egui::CursorIcon::Crosshair,
                None if state.tool == TimelineTool::Range => egui::CursorIcon::Crosshair,
                None => egui::CursorIcon::Default,
            });
        }
    }
    if let Some(keyframe) = hit_color_keyframe {
        let parameter = color_parameter_label(state.language, keyframe.parameter);
        let timeline_tick = state
            .timeline
            .clip(keyframe.clip_id)
            .and_then(|clip| color_keyframe_timeline_tick(clip, keyframe.source_tick))
            .unwrap_or(state.playhead);
        response.show_tooltip_text(format!(
            "{parameter} · {}\n{}",
            format_timecode_at_frame_rate(timeline_tick, state.frame_rate),
            t(
                state.language,
                "Drag horizontally to retime",
                "横にドラッグしてタイミングを変更"
            )
        ));
    }
    if response.secondary_clicked() {
        state.timeline_context_clip = hit_clip.map(|(_, clip_id)| clip_id);
        if let Some(clip_id) = state.timeline_context_clip
            && (state.selected_timeline_clip != Some(clip_id) || state.selected_title.is_some())
        {
            state.selected_timeline_clip = Some(clip_id);
            state.selected_title = None;
            state.mark_durable_edit();
        }
    }
    response.context_menu(|ui| {
        timeline_context_menu(ui, state);
    });
    timeline_pan_bar(ui, state);
    zoom_bar(ui, state);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{}: {}",
                t(state.language, "Frame", "フレーム"),
                format_timecode_at_frame_rate(state.playhead, state.frame_rate)
            ))
            .small()
            .monospace(),
        );
        ui.separator();
        ui.label(
            RichText::new(format!(
                "{}: {}",
                t(state.language, "View", "表示範囲"),
                format_seconds(state.visible_time_seconds())
            ))
            .small()
            .monospace(),
        );
    });
}

fn color_parameter_label(language: Language, parameter: ColorParameter) -> String {
    match parameter {
        ColorParameter::Temperature => t(language, "Temperature", "色温度"),
        ColorParameter::Tint => t(language, "Tint", "色かぶり"),
        ColorParameter::Saturation => t(language, "Saturation", "彩度"),
        ColorParameter::Exposure => t(language, "Exposure", "露出"),
        ColorParameter::Contrast => t(language, "Contrast", "コントラスト"),
        ColorParameter::Highlights => t(language, "Highlights", "ハイライト"),
        ColorParameter::Shadows => t(language, "Shadows", "シャドウ"),
        ColorParameter::Whites => t(language, "Whites", "白レベル"),
        ColorParameter::Blacks => t(language, "Blacks", "黒レベル"),
        ColorParameter::Brightness => t(language, "Brightness", "明るさ"),
        ColorParameter::VignetteAmount => t(language, "Vignette Amount", "ビネット適用量"),
        ColorParameter::VignetteMidpoint => t(language, "Vignette Midpoint", "ビネット中間点"),
        ColorParameter::VignetteFeather => t(language, "Vignette Feather", "ビネットぼかし"),
        ColorParameter::VignetteCenterX => t(language, "Vignette Center X", "ビネット中心 X"),
        ColorParameter::VignetteCenterY => t(language, "Vignette Center Y", "ビネット中心 Y"),
    }
}

fn clip_can_split_at_playhead(clip: &Clip, playhead: Tick) -> bool {
    clip.start < playhead && playhead < clip.end()
}

fn timeline_context_menu(ui: &mut Ui, state: &mut EditorState) {
    ui.set_min_width(240.0);
    ui.set_max_width(320.0);
    let target = state.timeline_context_clip.and_then(|clip_id| {
        let clip = state.timeline.clip(clip_id)?.clone();
        let track = state.timeline.track(clip.track_id)?;
        let media = state.media.iter().find(|item| item.id == clip.media.0)?;
        Some((clip, track.kind, media.id, media.display_name.clone()))
    });
    let Some((clip, kind, media_id, display_name)) = target else {
        if ui
            .button(t(state.language, "Add video track", "ビデオトラックを追加"))
            .clicked()
        {
            state.add_timeline_track(TrackKind::Video);
            ui.close();
        }
        if ui
            .button(t(
                state.language,
                "Add audio track",
                "オーディオトラックを追加",
            ))
            .clicked()
        {
            state.add_timeline_track(TrackKind::Audio);
            ui.close();
        }
        return;
    };

    let track_label = match kind {
        TrackKind::Video => t(state.language, "Video", "ビデオ"),
        TrackKind::Audio => t(state.language, "Audio", "オーディオ"),
    };
    ui.add_sized(
        [ui.available_width(), 18.0],
        egui::Label::new(
            RichText::new(format!("{track_label} · {display_name}"))
                .small()
                .color(Color32::from_rgb(151, 171, 185)),
        )
        .truncate(),
    )
    .on_hover_text(&display_name);
    ui.separator();
    timeline_context_open_menu(ui, state, kind, media_id);
    timeline_context_edit_menu(ui, state, &clip);
    timeline_context_clip_menu(ui, state, &clip);
    match kind {
        TrackKind::Video => timeline_context_video_menu(ui, state, &clip),
        TrackKind::Audio => timeline_context_audio_menu(ui, state, &clip),
    }
}

fn timeline_context_open_menu(
    ui: &mut Ui,
    state: &mut EditorState,
    kind: TrackKind,
    media_id: MediaId,
) {
    ui.menu_button(t(state.language, "Open", "開く"), |ui| {
        if ui
            .button(t(state.language, "Inspector", "インスペクタ"))
            .clicked()
        {
            select_right_sidebar_tab(state, RightSidebarTab::Inspector);
            ui.close();
        }
        let (destination, label) = match kind {
            TrackKind::Audio => (
                RightSidebarTab::Audio,
                t(state.language, "Audio", "オーディオ"),
            ),
            TrackKind::Video => (RightSidebarTab::Color, t(state.language, "Color", "カラー")),
        };
        if ui.button(label).clicked() {
            select_right_sidebar_tab(state, destination);
            ui.close();
        }
        if kind == TrackKind::Video
            && ui
                .button(t(state.language, "Effects", "エフェクト"))
                .clicked()
        {
            select_right_sidebar_tab(state, RightSidebarTab::Effects);
            ui.close();
        }
        ui.separator();
        if ui
            .button(t(
                state.language,
                "Source in Media",
                "メディアでソースを表示",
            ))
            .clicked()
        {
            if state.selected_media != Some(media_id) {
                state.selected_media = Some(media_id);
                state.mark_durable_edit();
            }
            select_right_sidebar_tab(state, RightSidebarTab::Media);
            ui.close();
        }
    });
}

fn timeline_context_edit_menu(ui: &mut Ui, state: &mut EditorState, clip: &Clip) {
    ui.menu_button(t(state.language, "Edit", "編集"), |ui| {
        if ui
            .add_enabled(
                clip_can_split_at_playhead(clip, state.playhead),
                egui::Button::new(t(state.language, "Split at Playhead", "再生ヘッドで分割")),
            )
            .clicked()
        {
            state.selected_timeline_clip = Some(clip.id);
            state.razor_at_playhead();
            state.timeline_context_clip = None;
            ui.close();
        }
        if ui
            .button(t(state.language, "Delete Clip", "クリップを削除"))
            .clicked()
        {
            state.selected_timeline_clip = Some(clip.id);
            state.delete_selected_timeline_clip();
            state.timeline_context_clip = None;
            ui.close();
        }
    });
}

fn timeline_context_clip_menu(ui: &mut Ui, state: &mut EditorState, clip: &Clip) {
    ui.menu_button(t(state.language, "Clip", "クリップ"), |ui| {
        let mut enabled = clip.enabled;
        if ui
            .checkbox(&mut enabled, t(state.language, "Enabled", "有効"))
            .on_hover_text(t(
                state.language,
                "Disabled clips stay in place but do not play or export.",
                "無効なクリップは位置を保持しますが、再生や書き出しには含まれません。",
            ))
            .changed()
        {
            state.set_timeline_clip_enabled(clip.id, enabled);
        }
        if ui
            .checkbox(
                &mut state.linked_selection,
                t(state.language, "Linked Selection", "リンク選択"),
            )
            .changed()
        {
            state.mark_durable_edit();
        }
    });
}

fn timeline_context_video_menu(ui: &mut Ui, state: &mut EditorState, clip: &Clip) {
    ui.menu_button(t(state.language, "Video", "ビデオ"), |ui| {
        let can_add_effect = clip.video_effects.len() < MAX_VIDEO_EFFECTS_PER_CLIP
            && next_video_effect_id(&clip.video_effects).is_some();
        ui.add_enabled_ui(can_add_effect, |ui| {
            ui.menu_button(t(state.language, "Add Effect", "エフェクトを追加"), |ui| {
                if ui
                    .button(t(state.language, "Basic Correction", "基本補正"))
                    .clicked()
                {
                    add_video_effect(
                        state,
                        clip,
                        VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
                    );
                    select_right_sidebar_tab(state, RightSidebarTab::Color);
                    ui.close();
                }
                if ui
                    .button(t(state.language, "Vignette", "ビネット"))
                    .clicked()
                {
                    add_video_effect(
                        state,
                        clip,
                        VideoEffectKind::Vignette(VignetteEffect::default()),
                    );
                    select_right_sidebar_tab(state, RightSidebarTab::Color);
                    ui.close();
                }
            });
        });
        ui.separator();
        for (edge, label) in [
            (
                FadeEdge::In,
                t(
                    state.language,
                    "Transition at Start",
                    "先頭のトランジション",
                ),
            ),
            (
                FadeEdge::Out,
                t(state.language, "Transition at End", "末尾のトランジション"),
            ),
        ] {
            ui.menu_button(label, |ui| {
                timeline_context_transition_edge_menu(ui, state, clip.id, edge);
            });
        }
    });
}

fn timeline_context_transition_edge_menu(
    ui: &mut Ui,
    state: &mut EditorState,
    clip_id: ClipId,
    edge: FadeEdge,
) {
    for (english, japanese, kinds) in [
        (
            "Dissolve",
            "ディゾルブ",
            &[
                VideoTransitionKind::CrossDissolve,
                VideoTransitionKind::FilmDissolve,
            ][..],
        ),
        (
            "Fade",
            "フェード",
            &[
                VideoTransitionKind::DipToBlack,
                VideoTransitionKind::DipToWhite,
            ][..],
        ),
        (
            "Wipe",
            "ワイプ",
            &[
                VideoTransitionKind::WipeLeft,
                VideoTransitionKind::WipeRight,
                VideoTransitionKind::WipeUp,
                VideoTransitionKind::WipeDown,
            ][..],
        ),
        (
            "Slide",
            "スライド",
            &[
                VideoTransitionKind::SlideFromLeft,
                VideoTransitionKind::SlideFromRight,
                VideoTransitionKind::SlideFromTop,
                VideoTransitionKind::SlideFromBottom,
            ][..],
        ),
    ] {
        ui.menu_button(t(state.language, english, japanese), |ui| {
            for kind in kinds {
                let available = state.can_add_video_transition(clip_id, edge, *kind);
                if ui
                    .add_enabled(
                        available,
                        egui::Button::new(video_transition_kind_label(state.language, *kind)),
                    )
                    .clicked()
                {
                    state.selected_timeline_clip = Some(clip_id);
                    state.add_video_transition(edge, *kind);
                    ui.close();
                }
            }
        });
    }
}

fn timeline_context_audio_menu(ui: &mut Ui, state: &mut EditorState, clip: &Clip) {
    ui.menu_button(t(state.language, "Audio", "オーディオ"), |ui| {
        ui.menu_button(
            t(state.language, "Crossfade", "クロスフェード"),
            |ui| {
                for (edge, english, japanese) in [
                    (FadeEdge::In, "At Start", "先頭"),
                    (FadeEdge::Out, "At End", "末尾"),
                ] {
                    let cut = state.adjacent_audio_cut(clip.id, edge);
                    let existing = cut.is_some_and(|(_, left, right)| {
                        state.timeline.audio_transitions().iter().any(|transition| {
                            transition.left_clip == left && transition.right_clip == right
                        })
                    });
                    let available = existing
                        || cut.is_some_and(|(_, left, right)| {
                            state
                                .audio_transition_duration_capacity(left, right, None)
                                .is_some_and(|capacity| capacity.0 > 0)
                        });
                    let action = if existing {
                        t(state.language, "Remove", "削除")
                    } else {
                        t(state.language, "Add Equal-Power", "イコールパワーを追加")
                    };
                    if ui
                        .add_enabled(
                            available,
                            egui::Button::new(format!(
                                "{} · {action}",
                                t(state.language, english, japanese)
                            )),
                        )
                        .clicked()
                    {
                        state.toggle_audio_crossfade(clip.id, edge);
                        ui.close();
                    }
                }
            },
        );
    });
}

const DEFAULT_TIMELINE_TRACK_HEIGHT: f32 = 32.0;
const THIRD_PARTY_NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES.md");
const MARKER_COLORS: [Color32; 5] = [
    Color32::from_rgb(239, 77, 81),
    Color32::from_rgb(238, 177, 69),
    Color32::from_rgb(83, 191, 126),
    Color32::from_rgb(78, 163, 226),
    Color32::from_rgb(185, 112, 219),
];
const MIN_TIMELINE_TRACK_HEIGHT: f32 = 28.0;
const MAX_TIMELINE_TRACK_HEIGHT: f32 = 320.0;
/// The lower half sits outside the clip's 3 px row inset, preserving clip controls.
const TRACK_RESIZE_GRIP_INSET: f32 = 2.0;
const TRACK_RESIZE_GRIP_OUTSET: f32 = 7.0;
const PLAYHEAD_HANDLE_WIDTH: f32 = 16.0;
const TIMELINE_NAVIGATOR_MIN_THUMB_WIDTH: f32 = 32.0;
const TIMELINE_NAVIGATOR_MIN_HEADROOM: i64 = 5_000_000;
const TIMELINE_NAVIGATOR_MAX_HEADROOM: i64 = 120_000_000;
const QUIET_WAVEFORM_THRESHOLD: f32 = 0.14;
const QUIET_WAVEFORM_TARGET: f32 = 0.62;
const MAX_WAVEFORM_DISPLAY_SCALE: f32 = 1_000_000.0;
const MIN_TIMELINE_CLIP_LABEL_WIDTH: f32 = 56.0;
const MIN_TIMELINE_WAVEFORM_STATUS_WIDTH: f32 = 112.0;

fn marker_color(index: u8) -> Color32 {
    MARKER_COLORS[index as usize % MARKER_COLORS.len()]
}

fn timeline_clip_label_is_visible(rect: Rect) -> bool {
    rect.width() >= MIN_TIMELINE_CLIP_LABEL_WIDTH && rect.height() >= 18.0
}

fn timeline_waveform_status_is_visible(rect: Rect) -> bool {
    rect.width() >= MIN_TIMELINE_WAVEFORM_STATUS_WIDTH && rect.height() >= 18.0
}

fn cached_timeline_status_galley(
    painter: &egui::Painter,
    cache: &mut Option<Arc<egui::Galley>>,
    language: Language,
    english: &'static str,
    japanese: &'static str,
) -> Arc<egui::Galley> {
    cache
        .get_or_insert_with(|| {
            painter.layout_no_wrap(
                t(language, english, japanese),
                FontId::proportional(10.0),
                Color32::WHITE,
            )
        })
        .clone()
}

fn cached_timeline_label_galley(
    painter: &egui::Painter,
    cache: &mut Vec<Option<Arc<egui::Galley>>>,
    media_index: usize,
    display_name: &str,
) -> Arc<egui::Galley> {
    if cache.len() <= media_index {
        cache.resize_with(media_index + 1, || None);
    }
    cache[media_index]
        .get_or_insert_with(|| {
            painter.layout_no_wrap(
                display_name.to_owned(),
                FontId::proportional(10.0),
                Color32::WHITE,
            )
        })
        .clone()
}

fn solid_line(
    canvas: &mut dyn TimelineCanvas,
    from: Pos2,
    to: Pos2,
    thickness: f32,
    color: Color32,
) {
    let thickness = thickness.max(1.0);
    let center = Pos2::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
    if (from.x - to.x).abs() <= f32::EPSILON {
        canvas.solid_rect(
            Rect::from_center_size(center, Vec2::new(thickness, (to.y - from.y).abs())),
            color,
        );
    } else if (from.y - to.y).abs() <= f32::EPSILON {
        canvas.solid_rect(
            Rect::from_center_size(center, Vec2::new((to.x - from.x).abs(), thickness)),
            color,
        );
    }
}

/// A compact allocation-free flag shape that stays in the overlay above native thumbnail tiles.
fn timeline_flag_rects(clip_rect: Rect) -> Option<[Rect; 2]> {
    if clip_rect.width() < 7.0 || clip_rect.height() < 7.0 {
        return None;
    }
    let origin = clip_rect.left_top() + Vec2::new(4.0, 1.0);
    let pole = Rect::from_min_size(origin, Vec2::new(1.5, 8.0)).intersect(clip_rect);
    let banner = Rect::from_min_size(origin, Vec2::new(8.0, 4.5)).intersect(clip_rect);
    (pole.is_positive() && banner.is_positive()).then_some([pole, banner])
}

fn solid_rect_stroke(canvas: &mut dyn TimelineCanvas, rect: Rect, thickness: f32, color: Color32) {
    let thickness = thickness
        .max(1.0)
        .min(rect.width() * 0.5)
        .min(rect.height() * 0.5);
    canvas.solid_rect(
        Rect::from_min_size(rect.min, Vec2::new(rect.width(), thickness)),
        color,
    );
    canvas.solid_rect(
        Rect::from_min_size(
            Pos2::new(rect.left(), rect.bottom() - thickness),
            Vec2::new(rect.width(), thickness),
        ),
        color,
    );
    canvas.solid_rect(
        Rect::from_min_size(rect.min, Vec2::new(thickness, rect.height())),
        color,
    );
    canvas.solid_rect(
        Rect::from_min_size(
            Pos2::new(rect.right() - thickness, rect.top()),
            Vec2::new(thickness, rect.height()),
        ),
        color,
    );
}

fn clamp_track_height(height: f32) -> f32 {
    height.clamp(MIN_TIMELINE_TRACK_HEIGHT, MAX_TIMELINE_TRACK_HEIGHT)
}

/// A full-width boundary makes resizing discoverable even when the track header is off-target.
fn track_resize_grip(row: Rect, timeline_viewport: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(
            timeline_viewport.left(),
            row.bottom() - TRACK_RESIZE_GRIP_INSET,
        ),
        Pos2::new(
            timeline_viewport.right(),
            row.bottom() + TRACK_RESIZE_GRIP_OUTSET,
        ),
    )
    .intersect(timeline_viewport)
}

/// The ruler handle is deliberately wider than the line, giving a reliable grab target while
/// keeping the red playhead visually precise.
fn playhead_handle_rect(playhead_x: f32, ruler: Rect) -> Rect {
    Rect::from_center_size(
        Pos2::new(playhead_x, ruler.center().y),
        Vec2::new(PLAYHEAD_HANDLE_WIDTH, ruler.height().max(1.0)),
    )
    .intersect(ruler)
}

fn draw_playhead_handle(canvas: &mut dyn TimelineCanvas, painter: &egui::Painter, handle: Rect) {
    let red = Color32::from_rgb(239, 77, 81);
    let body = Rect::from_center_size(
        Pos2::new(handle.center().x, handle.top() + 7.0),
        Vec2::new(10.0, 10.0),
    )
    .intersect(handle);
    canvas.solid_rect(body, red);
    painter.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(handle.center().x - 5.0, body.bottom()),
            Pos2::new(handle.center().x + 5.0, body.bottom()),
            Pos2::new(handle.center().x, handle.bottom()),
        ],
        red,
        Stroke::NONE,
    ));
}

/// Normalizes only the visual peak cache. Values are sanitized first, true silence remains
/// exactly flat, and a robust percentile prevents a few clipped buckets hiding quiet ambience.
fn normalize_waveform_display(peaks: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    let mut clean = Vec::with_capacity(peaks.len());
    let mut magnitudes = Vec::with_capacity(peaks.len() * 2);
    for (low, high) in peaks {
        let low = if low.is_finite() {
            low.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let high = if high.is_finite() {
            high.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let (low, high) = if low <= high {
            (low, high)
        } else {
            (high, low)
        };
        magnitudes.extend([low.abs(), high.abs()]);
        clean.push((low, high));
    }
    let mut nonzero: Vec<f32> = magnitudes
        .into_iter()
        .filter(|value| *value > 0.0)
        .collect();
    if nonzero.is_empty() {
        return clean;
    }
    nonzero.sort_by(f32::total_cmp);
    let reference = nonzero[((nonzero.len() - 1) as f32 * 0.80) as usize];
    if !reference.is_finite() || reference >= QUIET_WAVEFORM_THRESHOLD {
        return clean;
    }
    let scale = (QUIET_WAVEFORM_TARGET / reference).min(MAX_WAVEFORM_DISPLAY_SCALE);
    if !scale.is_finite() {
        return clean;
    }
    clean
        .into_iter()
        .map(|(low, high)| {
            (
                (low * scale).clamp(-1.0, 1.0),
                (high * scale).clamp(-1.0, 1.0),
            )
        })
        .collect()
}

/// Zero dB stays centered while the supported boost and attenuation ranges each use their
/// available half of the row. This keeps the gain line readable at every track height.
fn audio_gain_y(rect: Rect, gain_db: f32) -> f32 {
    let center = rect.center().y;
    let half_height = (rect.height() * 0.5 - 2.0).max(1.0);
    if gain_db >= 0.0 {
        center - gain_db.clamp(0.0, MAX_GAIN_DB) / MAX_GAIN_DB * half_height
    } else {
        center + gain_db.clamp(MIN_GAIN_DB, 0.0) / MIN_GAIN_DB * half_height
    }
}

fn audio_gain_at_y(rect: Rect, y: f32) -> f32 {
    let center = rect.center().y;
    let half_height = (rect.height() * 0.5 - 2.0).max(1.0);
    let delta = (center - y).clamp(-half_height, half_height);
    if delta >= 0.0 {
        delta / half_height * MAX_GAIN_DB
    } else {
        -(-delta / half_height * -MIN_GAIN_DB)
    }
}

fn gain_readout_rect(viewport: Rect, pointer: Pos2) -> Rect {
    let size = Vec2::new(74.0, 24.0);
    let mut min = pointer + Vec2::new(12.0, -size.y - 8.0);
    if min.y < viewport.top() {
        min.y = pointer.y + 8.0;
    }
    min.x = min.x.clamp(
        viewport.left(),
        (viewport.right() - size.x).max(viewport.left()),
    );
    min.y = min.y.clamp(
        viewport.top(),
        (viewport.bottom() - size.y).max(viewport.top()),
    );
    Rect::from_min_size(min, size)
}

fn draw_gain_readout(painter: &egui::Painter, viewport: Rect, pointer: Pos2, gain_db: f32) {
    let rect = gain_readout_rect(viewport, pointer);
    painter.rect_filled(rect, 3.0, Color32::from_rgb(17, 24, 31));
    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, Color32::from_rgb(112, 221, 166)),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{gain_db:+.1} dB"),
        FontId::monospace(11.0),
        Color32::from_rgb(235, 248, 238),
    );
}

/// Returns total height, maximum scroll and a scroll position that never exposes space below
/// the final row. Kept pure so variable row geometry is regression-testable.
fn track_layout(heights: &[f32], viewport_height: f32, scroll_y: f32) -> (f32, f32, f32) {
    let viewport_height = viewport_height.max(1.0);
    let total_height = heights.iter().copied().sum::<f32>();
    let max_scroll_y = (total_height - viewport_height).max(0.0);
    (
        total_height,
        max_scroll_y,
        scroll_y.clamp(0.0, max_scroll_y),
    )
}

fn track_row_at_y(heights: &[f32], content_top: f32, scroll_y: f32, y: f32) -> Option<usize> {
    let mut top = content_top - scroll_y;
    for (index, height) in heights.iter().enumerate() {
        if y >= top && y < top + height {
            return Some(index);
        }
        top += height;
    }
    None
}

fn visible_track_range(
    heights: &[f32],
    content_top: f32,
    scroll_y: f32,
    viewport_height: f32,
) -> std::ops::Range<usize> {
    let first =
        track_row_at_y(heights, content_top, scroll_y, content_top).unwrap_or(heights.len());
    let last = track_row_at_y(
        heights,
        content_top,
        scroll_y,
        content_top + viewport_height - 0.01,
    )
    .map(|index| index + 1)
    .unwrap_or(heights.len());
    first..last
}

fn draw_track_scrollbar(
    canvas: &mut dyn TimelineCanvas,
    viewport: Rect,
    total_height: f32,
    scroll_y: f32,
) {
    if total_height <= viewport.height() {
        return;
    }
    let max_scroll_y = (total_height - viewport.height()).max(1.0);
    let thumb_height =
        (viewport.height() * viewport.height() / total_height).clamp(22.0, viewport.height());
    let travel = (viewport.height() - thumb_height).max(0.0);
    let y = viewport.top() + travel * (scroll_y / max_scroll_y).clamp(0.0, 1.0);
    let thumb = Rect::from_min_size(
        Pos2::new(viewport.right() - 5.0, y),
        Vec2::new(3.0, thumb_height),
    );
    canvas.solid_rect(
        Rect::from_min_size(
            Pos2::new(viewport.right() - 5.0, viewport.top()),
            Vec2::new(3.0, viewport.height()),
        ),
        Color32::from_rgb(25, 34, 43),
    );
    canvas.solid_rect(thumb, Color32::from_rgb(81, 116, 136));
}

fn clip_rect_for(
    row: Rect,
    content: Rect,
    start: Tick,
    duration: Tick,
    view_start: Tick,
    visible_ticks: f32,
) -> Rect {
    let x = content.left() + (start.0 - view_start.0) as f32 / visible_ticks * content.width();
    let width = duration.0 as f32 / visible_ticks * content.width();
    Rect::from_min_max(
        Pos2::new(x.max(content.left()), row.top() + 3.0),
        Pos2::new(
            (x + width).min(content.right()).max(content.left()),
            row.bottom() - 3.0,
        ),
    )
}

const COLOR_KEYFRAME_MARKER_RADIUS: f32 = 3.5;
const COLOR_KEYFRAME_LANES: usize = 10;
const MIN_COLOR_KEYFRAME_CLIP_HEIGHT: f32 = 26.0;

fn active_color_effect_for_clip(state: &EditorState, clip: &Clip) -> Option<VideoEffectId> {
    state
        .active_color_effect
        .filter(|id| clip.video_effects.iter().any(|node| node.id == *id))
        .or_else(|| clip.video_effects.first().map(|node| node.id))
}

fn color_keyframe_timeline_tick(clip: &Clip, source_tick: Tick) -> Option<Tick> {
    let source_end = clip.source_in.0.saturating_add(clip.duration.0);
    (source_tick.0 >= clip.source_in.0 && source_tick.0 <= source_end).then(|| {
        Tick(
            clip.start
                .0
                .saturating_add(source_tick.0 - clip.source_in.0),
        )
    })
}

fn color_keyframe_marker_center(
    clip_rect: Rect,
    content: Rect,
    view_start: Tick,
    visible_ticks: f32,
    clip: &Clip,
    parameter: ColorParameter,
    source_tick: Tick,
) -> Option<Pos2> {
    let timeline_tick = color_keyframe_timeline_tick(clip, source_tick)?;
    let x = content.left()
        + (timeline_tick.0 - view_start.0) as f32 / visible_ticks.max(1.0) * content.width();
    if x < clip_rect.left() || x > clip_rect.right() {
        return None;
    }
    Some(Pos2::new(x, color_keyframe_lane_y(clip_rect, parameter)))
}

fn color_keyframe_lane_y(clip_rect: Rect, parameter: ColorParameter) -> f32 {
    let inset = COLOR_KEYFRAME_MARKER_RADIUS + 1.0;
    let top = clip_rect.top() + inset;
    let bottom = (clip_rect.bottom() - inset).max(top);
    let fraction = color_keyframe_lane(parameter) as f32 / (COLOR_KEYFRAME_LANES - 1) as f32;
    top + (bottom - top) * fraction
}

fn color_keyframe_lane(parameter: ColorParameter) -> usize {
    match parameter {
        ColorParameter::Temperature => 0,
        ColorParameter::Tint => 1,
        ColorParameter::Saturation => 2,
        ColorParameter::Exposure => 3,
        ColorParameter::Contrast => 4,
        ColorParameter::Highlights => 5,
        ColorParameter::Shadows => 6,
        ColorParameter::Whites => 7,
        ColorParameter::Blacks => 8,
        ColorParameter::Brightness => 9,
        ColorParameter::VignetteAmount => 0,
        ColorParameter::VignetteMidpoint => 1,
        ColorParameter::VignetteFeather => 2,
        ColorParameter::VignetteCenterX => 3,
        ColorParameter::VignetteCenterY => 4,
    }
}

fn color_parameter_timeline_color(parameter: ColorParameter) -> Color32 {
    match parameter {
        ColorParameter::Temperature => Color32::from_rgb(101, 181, 229),
        ColorParameter::Tint => Color32::from_rgb(211, 122, 183),
        ColorParameter::Saturation => Color32::from_rgb(93, 208, 239),
        ColorParameter::Exposure => Color32::from_rgb(220, 186, 92),
        ColorParameter::Contrast => Color32::from_rgb(130, 198, 241),
        ColorParameter::Highlights => Color32::from_rgb(245, 222, 131),
        ColorParameter::Shadows => Color32::from_rgb(142, 133, 211),
        ColorParameter::Whites => Color32::from_rgb(239, 239, 205),
        ColorParameter::Blacks => Color32::from_rgb(94, 109, 124),
        ColorParameter::Brightness => Color32::from_rgb(246, 188, 72),
        ColorParameter::VignetteAmount => Color32::from_rgb(193, 135, 232),
        ColorParameter::VignetteMidpoint => Color32::from_rgb(116, 185, 222),
        ColorParameter::VignetteFeather => Color32::from_rgb(144, 207, 197),
        ColorParameter::VignetteCenterX => Color32::from_rgb(230, 167, 105),
        ColorParameter::VignetteCenterY => Color32::from_rgb(229, 114, 144),
    }
}

fn draw_timeline_color_keyframes(
    painter: &egui::Painter,
    clip_rect: Rect,
    content: Rect,
    view_start: Tick,
    visible_ticks: f32,
    clip: &Clip,
    node: &VideoEffectNode,
) {
    if clip_rect.height() < MIN_COLOR_KEYFRAME_CLIP_HEIGHT {
        return;
    }
    let lanes = video_effect_scalars(&node.kind);
    let visible_key_count = lanes
        .iter()
        .flatten()
        .map(|(_, scalar)| scalar.keyframes.len())
        .sum::<usize>();
    let mut markers = egui::Mesh::default();
    markers
        .vertices
        .reserve(visible_key_count.saturating_mul(4));
    markers.indices.reserve(visible_key_count.saturating_mul(6));
    for (parameter, scalar) in lanes.into_iter().flatten() {
        let y = color_keyframe_lane_y(clip_rect, parameter);
        let color = color_parameter_timeline_color(parameter);
        painter.line_segment(
            [
                Pos2::new(clip_rect.left(), y),
                Pos2::new(clip_rect.right(), y),
            ],
            Stroke::new(
                1.0,
                Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 120),
            ),
        );
        for key in &scalar.keyframes {
            let Some(center) = color_keyframe_marker_center(
                clip_rect,
                content,
                view_start,
                visible_ticks,
                clip,
                parameter,
                key.source_tick,
            ) else {
                continue;
            };
            let first = markers.vertices.len() as u32;
            for point in [
                Pos2::new(center.x, center.y - COLOR_KEYFRAME_MARKER_RADIUS),
                Pos2::new(center.x + COLOR_KEYFRAME_MARKER_RADIUS, center.y),
                Pos2::new(center.x, center.y + COLOR_KEYFRAME_MARKER_RADIUS),
                Pos2::new(center.x - COLOR_KEYFRAME_MARKER_RADIUS, center.y),
            ] {
                markers.colored_vertex(point, color);
            }
            markers.add_triangle(first, first + 1, first + 2);
            markers.add_triangle(first, first + 2, first + 3);
        }
    }
    if !markers.is_empty() {
        painter.add(markers);
    }
}

fn color_keyframe_hit(
    clip_rect: Rect,
    content: Rect,
    view_start: Tick,
    visible_ticks: f32,
    clip: &Clip,
    node: &VideoEffectNode,
    pointer: Pos2,
) -> Option<TimelineColorKeyframe> {
    if clip_rect.height() < MIN_COLOR_KEYFRAME_CLIP_HEIGHT {
        return None;
    }
    for (parameter, scalar) in video_effect_scalars(&node.kind).into_iter().flatten() {
        for key in &scalar.keyframes {
            let Some(center) = color_keyframe_marker_center(
                clip_rect,
                content,
                view_start,
                visible_ticks,
                clip,
                parameter,
                key.source_tick,
            ) else {
                continue;
            };
            if pointer.distance(center) <= COLOR_KEYFRAME_MARKER_RADIUS + 3.0 {
                let pointer_timeline_tick =
                    timeline_tick_at(pointer.x, content, view_start, visible_ticks);
                let pointer_source_tick = Tick(
                    clip.source_in
                        .0
                        .saturating_add(pointer_timeline_tick.0.saturating_sub(clip.start.0)),
                );
                return Some(TimelineColorKeyframe {
                    clip_id: clip.id,
                    effect_id: node.id,
                    parameter,
                    source_tick: key.source_tick,
                    grab_offset: Tick(pointer_source_tick.0.saturating_sub(key.source_tick.0)),
                    value: key.value,
                    interpolation: key.interpolation,
                });
            }
        }
    }
    None
}

fn retime_color_keyframe(
    timeline: &mut Timeline,
    keyframe: TimelineColorKeyframe,
    target_source_tick: Tick,
) -> bool {
    if target_source_tick == keyframe.source_tick
        || timeline
            .color_keyframe(
                keyframe.clip_id,
                keyframe.effect_id,
                keyframe.parameter,
                target_source_tick,
            )
            .is_some()
    {
        return false;
    }
    if timeline
        .remove_color_keyframe(
            keyframe.clip_id,
            keyframe.effect_id,
            keyframe.parameter,
            keyframe.source_tick,
        )
        .ok()
        != Some(true)
    {
        return false;
    }
    timeline
        .set_color_keyframe(
            keyframe.clip_id,
            keyframe.effect_id,
            keyframe.parameter,
            target_source_tick,
            keyframe.value,
            keyframe.interpolation,
        )
        .is_ok()
}

fn video_strip_frame_count(layout: VideoStripLayout) -> usize {
    layout
        .frame_count
        .max(1)
        .min(layout.columns.max(1).saturating_mul(layout.rows.max(1)))
}

/// Chooses the nearest FFmpeg atlas sample. `extract_video_strip` samples at
/// `frame_count / duration`, so its source positions are `index * duration / frame_count`.
fn video_strip_frame_index(layout: VideoStripLayout, source_tick: Tick) -> usize {
    let frames = video_strip_frame_count(layout);
    if frames <= 1 || layout.duration.0 <= 0 {
        return 0;
    }
    let source = source_tick.0.clamp(0, layout.duration.0) as f64;
    let index = (source * frames as f64 / layout.duration.0 as f64).round() as usize;
    index.min(frames - 1)
}

/// Returns an in-bounds row-major UV rectangle for one usable atlas cell.
fn video_strip_frame_uv(layout: VideoStripLayout, frame: usize) -> Rect {
    let columns = layout.columns.max(1);
    let rows = layout.rows.max(1);
    let index = frame.min(video_strip_frame_count(layout) - 1);
    let column = index % columns;
    let row = (index / columns).min(rows - 1);
    let min = Pos2::new(column as f32 / columns as f32, row as f32 / rows as f32);
    let max = Pos2::new(
        (column + 1) as f32 / columns as f32,
        (row + 1) as f32 / rows as f32,
    );
    Rect::from_min_max(min, max)
}

fn video_strip_aspect(layout: VideoStripLayout) -> f32 {
    if layout.frame_width == 0 || layout.frame_height == 0 {
        16.0 / 9.0
    } else {
        layout.frame_width as f32 / layout.frame_height as f32
    }
}

fn draw_video_strip_frame(
    painter: &egui::Painter,
    rect: Rect,
    strip: CachedVideoStrip,
    frame: usize,
) {
    draw_video_strip_frame_tinted(painter, rect, strip, frame, Color32::WHITE);
}

fn draw_video_strip_frame_tinted(
    painter: &egui::Painter,
    rect: Rect,
    strip: CachedVideoStrip,
    frame: usize,
    tint: Color32,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    painter.image(
        strip.texture,
        rect,
        video_strip_frame_uv(strip.layout, frame),
        tint,
    );
}

/// Paints only a bounded number of 16:9 atlas cells, preserving each thumbnail instead of
/// stretching the whole grid across the clip.
fn draw_timeline_video_tiles(
    canvas: &mut dyn TimelineCanvas,
    rect: Rect,
    clip: &nle_timeline::Clip,
    strip: CachedVideoStrip,
) {
    const MAX_TIMELINE_VIDEO_TILES: usize = 12;
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let tile_width = (rect.height() * video_strip_aspect(strip.layout)).max(1.0);
    let tile_count =
        ((rect.width() / tile_width).ceil() as usize).clamp(1, MAX_TIMELINE_VIDEO_TILES);
    let last_left = (rect.right() - tile_width).max(rect.left());
    for index in 0..tile_count {
        let position = if tile_count == 1 {
            rect.left()
        } else {
            egui::lerp(
                rect.left()..=last_left,
                index as f32 / (tile_count - 1) as f32,
            )
        };
        let tile = Rect::from_min_size(
            Pos2::new(position, rect.top()),
            Vec2::new(tile_width, rect.height()),
        );
        let normalized = (index as f32 + 0.5) / tile_count as f32;
        let source_tick = Tick(
            clip.source_in
                .0
                .saturating_add((clip.duration.0.max(0) as f32 * normalized).round() as i64),
        );
        let visible_tile = tile.intersect(rect);
        if !visible_tile.is_positive() {
            continue;
        }
        let uv = video_strip_frame_uv(
            strip.layout,
            video_strip_frame_index(strip.layout, source_tick),
        );
        let clipped_uv = clip_texture_uv(uv, tile, visible_tile);
        canvas.texture_rect(
            visible_tile,
            strip.native_texture_id,
            strip.texture,
            clipped_uv,
            Color32::WHITE,
        );
    }
}

/// Applies destination clipping to a texture UV rectangle. Timeline texture callbacks do not
/// have per-clip scissor state, so this produces the same visible atlas crop that egui's clipped
/// painter previously produced.
fn clip_texture_uv(uv: Rect, full_rect: Rect, visible_rect: Rect) -> Rect {
    let width = full_rect.width().max(f32::EPSILON);
    let height = full_rect.height().max(f32::EPSILON);
    let left = (visible_rect.left() - full_rect.left()) / width;
    let right = (visible_rect.right() - full_rect.left()) / width;
    let top = (visible_rect.top() - full_rect.top()) / height;
    let bottom = (visible_rect.bottom() - full_rect.top()) / height;
    Rect::from_min_max(
        Pos2::new(
            egui::lerp(uv.left()..=uv.right(), left),
            egui::lerp(uv.top()..=uv.bottom(), top),
        ),
        Pos2::new(
            egui::lerp(uv.left()..=uv.right(), right),
            egui::lerp(uv.top()..=uv.bottom(), bottom),
        ),
    )
}

fn draw_timeline_clip(
    canvas: &mut dyn TimelineCanvas,
    painter: &egui::Painter,
    rect: Rect,
    kind: TrackKind,
    clip: &nle_timeline::Clip,
    paint: TimelineClipPaint<'_>,
) {
    let (fill, line) = timeline_clip_palette(kind, clip.media.0, paint.offline);
    canvas.solid_rect(rect, fill);
    if !paint.offline
        && kind == TrackKind::Video
        && paint.show_video_thumbnails
        && let Some(strip) = paint.video_strip
    {
        // The native canvas owns the title strip; leave it uncovered by egui texture tiles so
        // the callback remains below every overlay while the label stays readable.
        let tile_rect = Rect::from_min_max(
            Pos2::new(rect.left() + 1.0, (rect.top() + 16.0).min(rect.bottom())),
            Pos2::new(rect.right() - 1.0, rect.bottom() - 1.0),
        );
        draw_timeline_video_tiles(canvas, tile_rect, clip, strip);
        canvas.solid_rect(
            Rect::from_min_max(rect.left_top(), Pos2::new(rect.right(), rect.top() + 16.0)),
            Color32::from_black_alpha(120),
        );
    }
    solid_rect_stroke(
        canvas,
        rect,
        if paint.selected { 2.0 } else { 1.0 },
        if paint.selected { Color32::WHITE } else { line },
    );
    if let Some(label) = paint.label_galley {
        let label_painter = painter.with_clip_rect(rect.shrink(1.0));
        let color = Color32::from_rgb(225, 241, 248);
        let mut position = rect.left_top() + Vec2::new(6.0, 4.0);
        if let Some(prefix) = paint.offline_prefix_galley {
            let prefix_width = prefix.size().x;
            label_painter.galley(position, prefix, color);
            position.x += prefix_width;
        }
        label_painter.galley(position, label, color);
    }
    if kind == TrackKind::Audio {
        if paint.show_audio_waveforms
            && let Some(waveform) = paint.waveform.filter(|waveform| !waveform.peaks.is_empty())
        {
            draw_timeline_waveform(canvas, rect, &waveform.peaks, line);
        } else if paint.show_audio_waveforms
            && let Some(status) = paint.waveform_status_galley
        {
            let position = rect.center() - status.size() * 0.5;
            painter.with_clip_rect(rect.shrink(1.0)).galley(
                position,
                status,
                paint.waveform_status_color,
            );
        }
    }
    if let Some(color) = paint.flag_color
        && let Some(flag_rects) = timeline_flag_rects(rect)
    {
        for flag_rect in flag_rects {
            painter.rect_filled(flag_rect, 0.0, color);
        }
    }
    draw_fade(
        painter,
        rect,
        FadePaint {
            clip_duration: clip.duration,
            fade: clip.fade_in,
            edge: FadeEdge::In,
            kind,
            color: line,
        },
    );
    draw_fade(
        painter,
        rect,
        FadePaint {
            clip_duration: clip.duration,
            fade: clip.fade_out,
            edge: FadeEdge::Out,
            kind,
            color: line,
        },
    );
    if kind == TrackKind::Audio {
        let gain_y = audio_gain_y(rect, clip.gain_db);
        // Keep the gain rubber band in the retained timeline layer. A native canvas callback can
        // otherwise cover egui overlay geometry, leaving the waveform's partial zero baseline
        // looking like the control. Emitting this after the waveform makes the real envelope a
        // continuous, correctly aligned hit target across the full clip width.
        solid_line(
            canvas,
            Pos2::new(rect.left(), gain_y),
            Pos2::new(rect.right(), gain_y),
            1.5,
            Color32::from_rgb(235, 248, 238),
        );
    }
    if !paint.enabled {
        let disabled_rect = rect.shrink(1.0);
        let disabled_painter = painter.with_clip_rect(disabled_rect);
        disabled_painter.rect_filled(disabled_rect, 1.0, Color32::from_black_alpha(158));
        let stripe_color = Color32::from_rgba_unmultiplied(147, 163, 174, 70);
        let mut x = disabled_rect.left() - disabled_rect.height();
        while x < disabled_rect.right() {
            disabled_painter.line_segment(
                [
                    Pos2::new(x, disabled_rect.bottom()),
                    Pos2::new(x + disabled_rect.height(), disabled_rect.top()),
                ],
                Stroke::new(1.0, stripe_color),
            );
            x += 12.0;
        }
        disabled_painter.rect_stroke(
            disabled_rect,
            1.0,
            Stroke::new(1.0, Color32::from_rgb(105, 119, 129)),
            StrokeKind::Inside,
        );
    }
    if paint.show_handles {
        for (fade, edge) in [(clip.fade_in, FadeEdge::In), (clip.fade_out, FadeEdge::Out)] {
            let geometry = fade_control_geometry(rect, clip.duration, fade, edge);
            let point = geometry.full_endpoint;
            painter.circle_filled(point, 3.5, Color32::from_rgb(229, 240, 244));
            painter.circle_stroke(point, 3.5, Stroke::new(1.0, line));
        }
    }
}

const VIDEO_CLIP_PALETTES: [(Color32, Color32); 8] = [
    (
        Color32::from_rgb(20, 92, 135),
        Color32::from_rgb(113, 202, 242),
    ),
    (
        Color32::from_rgb(18, 102, 121),
        Color32::from_rgb(91, 215, 226),
    ),
    (
        Color32::from_rgb(45, 77, 137),
        Color32::from_rgb(133, 180, 242),
    ),
    (
        Color32::from_rgb(40, 91, 112),
        Color32::from_rgb(137, 203, 224),
    ),
    (
        Color32::from_rgb(16, 104, 143),
        Color32::from_rgb(94, 213, 242),
    ),
    (
        Color32::from_rgb(57, 68, 132),
        Color32::from_rgb(151, 170, 239),
    ),
    (
        Color32::from_rgb(31, 82, 123),
        Color32::from_rgb(120, 192, 232),
    ),
    (
        Color32::from_rgb(22, 99, 106),
        Color32::from_rgb(96, 213, 206),
    ),
];
const AUDIO_CLIP_PALETTES: [(Color32, Color32); 8] = [
    (
        Color32::from_rgb(35, 118, 82),
        Color32::from_rgb(112, 221, 166),
    ),
    (
        Color32::from_rgb(41, 109, 66),
        Color32::from_rgb(126, 215, 139),
    ),
    (
        Color32::from_rgb(27, 111, 99),
        Color32::from_rgb(92, 220, 194),
    ),
    (
        Color32::from_rgb(52, 105, 74),
        Color32::from_rgb(145, 209, 155),
    ),
    (
        Color32::from_rgb(27, 118, 68),
        Color32::from_rgb(99, 225, 144),
    ),
    (
        Color32::from_rgb(45, 104, 91),
        Color32::from_rgb(134, 210, 191),
    ),
    (
        Color32::from_rgb(34, 113, 55),
        Color32::from_rgb(115, 218, 125),
    ),
    (
        Color32::from_rgb(23, 108, 91),
        Color32::from_rgb(89, 211, 182),
    ),
];

fn media_palette_index(media_id: MediaId) -> usize {
    let mut hash = media_id.wrapping_mul(0x9E37_79B9);
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85EB_CA6B);
    hash ^= hash >> 13;
    hash as usize & (VIDEO_CLIP_PALETTES.len() - 1)
}

fn timeline_clip_palette(kind: TrackKind, media_id: MediaId, offline: bool) -> (Color32, Color32) {
    if offline {
        return (
            Color32::from_rgb(164, 35, 137),
            Color32::from_rgb(255, 126, 224),
        );
    }
    let index = media_palette_index(media_id);
    match kind {
        TrackKind::Video => VIDEO_CLIP_PALETTES[index],
        TrackKind::Audio => AUDIO_CLIP_PALETTES[index],
    }
}

fn draw_timeline_waveform(
    canvas: &mut dyn TimelineCanvas,
    rect: Rect,
    peaks: &[(f32, f32)],
    color: Color32,
) {
    let count = peaks.len().min(rect.width().max(1.0) as usize);
    if count == 0 {
        return;
    }
    let step = peaks.len() as f32 / count as f32;
    let center = rect.center().y;
    for index in 0..count {
        let (low, high) = peaks[((index as f32 * step) as usize).min(peaks.len() - 1)];
        let x = rect.left() + index as f32 / count as f32 * rect.width();
        let top = center - high.clamp(0.0, 1.0) * rect.height() * 0.42;
        let bottom = center - low.clamp(-1.0, 0.0) * rect.height() * 0.42;
        canvas.solid_rect(
            Rect::from_min_max(
                Pos2::new(x, top),
                Pos2::new((x + 1.0).min(rect.right()), bottom.max(top + 0.5)),
            ),
            color,
        );
    }
}

fn draw_waveform(painter: &egui::Painter, rect: Rect, peaks: &[(f32, f32)], color: Color32) {
    let count = peaks.len().min(rect.width().max(1.0) as usize);
    if count == 0 {
        return;
    }
    let step = peaks.len() as f32 / count as f32;
    let center = rect.center().y;
    for index in 0..count {
        let (low, high) = peaks[((index as f32 * step) as usize).min(peaks.len() - 1)];
        let x = rect.left() + index as f32 / count as f32 * rect.width();
        painter.line_segment(
            [
                Pos2::new(x, center - high.clamp(0.0, 1.0) * rect.height() * 0.42),
                Pos2::new(x, center - low.clamp(-1.0, 0.0) * rect.height() * 0.42),
            ],
            Stroke::new(1.0, color),
        );
    }
}

#[derive(Clone, Copy)]
struct FadePaint {
    clip_duration: Tick,
    fade: nle_timeline::Fade,
    edge: FadeEdge,
    kind: TrackKind,
    color: Color32,
}

fn draw_fade(painter: &egui::Painter, rect: Rect, paint: FadePaint) {
    if paint.fade.duration.0 <= 0 {
        return;
    }
    let geometry = fade_control_geometry(rect, paint.clip_duration, paint.fade, paint.edge);
    if paint.kind == TrackKind::Video {
        const STEPS: usize = 12;
        let left = geometry.outer_endpoint.x.min(geometry.full_endpoint.x);
        let right = geometry.outer_endpoint.x.max(geometry.full_endpoint.x);
        for step in 0..STEPS {
            let t0 = step as f32 / STEPS as f32;
            let t1 = (step + 1) as f32 / STEPS as f32;
            let opacity = match paint.edge {
                FadeEdge::In => 1.0 - t0,
                FadeEdge::Out => t1,
            };
            let x0 = left + (right - left) * t0;
            let x1 = left + (right - left) * t1;
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(x0, rect.top()), Pos2::new(x1, rect.bottom())),
                0.0,
                Color32::from_black_alpha((opacity * 190.0) as u8),
            );
        }
    }
    draw_fade_shading(
        painter,
        rect,
        geometry,
        Color32::from_black_alpha(if paint.kind == TrackKind::Audio {
            92
        } else {
            72
        }),
    );
    draw_fade_envelope(painter, geometry, paint.color);
    painter.circle_filled(geometry.curve_point, 3.3, Color32::WHITE);
}

/// Fills the attenuated side of the same curve used for hit testing. This makes a fade readable
/// before the pointer reaches its small handles and keeps audio/video fade cues consistent.
fn draw_fade_shading(
    painter: &egui::Painter,
    rect: Rect,
    geometry: FadeControlGeometry,
    color: Color32,
) {
    let mut mesh = egui::Mesh::default();
    let bands = fade_shade_bands(rect, geometry);
    for (top, curve) in &bands {
        mesh.colored_vertex(*top, color);
        mesh.colored_vertex(*curve, color);
    }
    for step in 0..bands.len().saturating_sub(1) as u32 {
        let top0 = step * 2;
        let curve0 = top0 + 1;
        let top1 = top0 + 2;
        let curve1 = top0 + 3;
        mesh.add_triangle(top0, curve0, top1);
        mesh.add_triangle(top1, curve0, curve1);
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn fade_shade_bands(rect: Rect, geometry: FadeControlGeometry) -> Vec<(Pos2, Pos2)> {
    const STEPS: usize = 20;
    (0..=STEPS)
        .map(|step| {
            let curve = fade_envelope_point(geometry, step as f32 / STEPS as f32);
            (Pos2::new(curve.x, rect.top()), curve)
        })
        .collect()
}

const FADE_CURVE_HEIGHT_FACTOR: f32 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq)]
struct FadeControlGeometry {
    /// The outer clip edge where audio is silent and video is black.
    outer_endpoint: Pos2,
    /// The inward boundary where the clip has reached full visibility / gain.
    full_endpoint: Pos2,
    /// A point on the rendered quadratic envelope, not a replacement for the full endpoint.
    curve_point: Pos2,
}

fn fade_control_geometry(
    rect: Rect,
    clip_duration: Tick,
    fade: nle_timeline::Fade,
    edge: FadeEdge,
) -> FadeControlGeometry {
    let fraction = (fade.duration.0 as f32 / clip_duration.0.max(1) as f32).clamp(0.0, 1.0);
    let fade_width = rect.width() * fraction;
    let (outer_x, full_x) = match edge {
        FadeEdge::In => (rect.left(), rect.left() + fade_width),
        FadeEdge::Out => (rect.right(), rect.right() - fade_width),
    };
    FadeControlGeometry {
        outer_endpoint: Pos2::new(outer_x, rect.bottom() - 2.0),
        full_endpoint: Pos2::new(full_x, rect.top() + 2.0),
        curve_point: Pos2::new((outer_x + full_x) * 0.5, fade_curve_y(rect, fade.curve)),
    }
}

fn fade_curve_y(rect: Rect, curve: f32) -> f32 {
    (rect.center().y - curve * rect.height() * FADE_CURVE_HEIGHT_FACTOR)
        .clamp(rect.top() + 2.0, rect.bottom() - 2.0)
}

fn fade_curve_at_y(rect: Rect, y: f32) -> f32 {
    ((rect.center().y - y) / (rect.height() * FADE_CURVE_HEIGHT_FACTOR).max(1.0)).clamp(-1.0, 1.0)
}

/// A quadratic Bézier passes through the draggable midpoint, making the drawn curve, hit point,
/// and drag mapping one shared piece of geometry.
fn fade_envelope_point(geometry: FadeControlGeometry, t: f32) -> Pos2 {
    let t = t.clamp(0.0, 1.0);
    let one_minus_t = 1.0 - t;
    // B(0.5) = 0.25 * P0 + 0.5 * C + 0.25 * P2. Solve C for the supplied midpoint.
    let control = geometry.curve_point.to_vec2() * 2.0
        - (geometry.outer_endpoint.to_vec2() + geometry.full_endpoint.to_vec2()) * 0.5;
    let point = geometry.outer_endpoint.to_vec2() * (one_minus_t * one_minus_t)
        + control * (2.0 * one_minus_t * t)
        + geometry.full_endpoint.to_vec2() * (t * t);
    Pos2::new(point.x, point.y)
}

fn draw_fade_envelope(painter: &egui::Painter, geometry: FadeControlGeometry, color: Color32) {
    const STEPS: usize = 20;
    let mut previous = geometry.outer_endpoint;
    for step in 1..=STEPS {
        let point = fade_envelope_point(geometry, step as f32 / STEPS as f32);
        painter.line_segment([previous, point], Stroke::new(1.3, color));
        previous = point;
    }
}

fn clip_drag_hit(
    rect: Rect,
    kind: TrackKind,
    clip: &nle_timeline::Clip,
    pointer: Pos2,
    visible_ticks: f32,
    content_width: f32,
) -> Option<TimelineDrag> {
    let _ = (visible_ticks, content_width);
    for (edge, fade) in [(FadeEdge::In, clip.fade_in), (FadeEdge::Out, clip.fade_out)] {
        let geometry = fade_control_geometry(rect, clip.duration, fade, edge);
        if pointer.distance(geometry.full_endpoint) <= 9.0 {
            return Some(TimelineDrag::FadeDuration(clip.id, edge));
        }
        if pointer.distance(geometry.curve_point) <= 7.0 && fade.duration.0 > 0 {
            return Some(TimelineDrag::FadeCurve(clip.id, edge));
        }
    }
    if kind == TrackKind::Audio && (pointer.y - audio_gain_y(rect, clip.gain_db)).abs() <= 5.0 {
        return Some(TimelineDrag::Gain(clip.id));
    }
    None
}

/// Separates ordinary hover and a drag's press origin from egui's canvas-owned interaction
/// pointer. `Some(None)` means the pointer is inside the clip body and should move the clip.
fn clip_hit_at_pointer(
    rect: Rect,
    kind: TrackKind,
    clip: &nle_timeline::Clip,
    pointer: Pos2,
    visible_ticks: f32,
    content_width: f32,
) -> Option<Option<TimelineDrag>> {
    rect.contains(pointer)
        .then(|| clip_drag_hit(rect, kind, clip, pointer, visible_ticks, content_width))
}

/// Structural trim lives on the vertical clip edges; fade controls keep the top strip.
fn clip_structural_edge_hit(rect: Rect, pointer: Pos2) -> Option<FadeEdge> {
    (pointer.y > rect.top() + 12.0).then(|| {
        if (pointer.x - rect.left()).abs() <= 7.0 {
            Some(FadeEdge::In)
        } else if (pointer.x - rect.right()).abs() <= 7.0 {
            Some(FadeEdge::Out)
        } else {
            None
        }
    })?
}

/// Finds the two clips sharing the edge being dragged. Roll is deliberately limited to an
/// exact adjoining edit on the same track; gaps remain ordinary ripple trims.
fn timeline_roll_pair(
    timeline: &Timeline,
    clip_id: ClipId,
    edge: FadeEdge,
) -> Option<(ClipId, ClipId)> {
    let clip = timeline.clip(clip_id)?;
    let track = timeline.track(clip.track_id)?;
    let index = track
        .clips
        .iter()
        .position(|candidate| candidate.id == clip_id)?;
    match edge {
        FadeEdge::In => track
            .clips
            .get(index.checked_sub(1)?)
            .filter(|left| left.end() == clip.start)
            .map(|left| (left.id, clip.id)),
        FadeEdge::Out => track
            .clips
            .get(index + 1)
            .filter(|right| right.start == clip.end())
            .map(|right| (clip.id, right.id)),
    }
}

fn tool_row(ui: &mut Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(state.history.can_undo(), egui::Button::new("↶"))
            .on_hover_text(t(state.language, "Undo (Ctrl+Z)", "元に戻す (Ctrl+Z)"))
            .clicked()
        {
            state.undo_timeline();
        }
        if ui
            .add_enabled(state.history.can_redo(), egui::Button::new("↷"))
            .on_hover_text(t(state.language, "Redo (Ctrl+Y)", "やり直す (Ctrl+Y)"))
            .clicked()
        {
            state.redo_timeline();
        }
        if ui
            .small_button("⌘")
            .on_hover_text(t(
                state.language,
                "Command palette (Ctrl+P)",
                "コマンドパレット (Ctrl+P)",
            ))
            .clicked()
        {
            state.command_palette_open = true;
            state.command_query.clear();
        }
        ui.separator();
        for (tool, icon, en, jp, key) in [
            (TimelineTool::Pointer, "↖", "Selection", "選択", "A"),
            (TimelineTool::Range, "▭", "Range", "範囲", "R"),
            (TimelineTool::Trim, "↔", "Trim", "トリム", "T"),
            (TimelineTool::Razor, "✂", "Blade", "ブレード", "B"),
            (
                TimelineTool::DynamicTrim,
                "◖◗",
                "Dynamic Trim",
                "ダイナミックトリム",
                "W",
            ),
            (TimelineTool::Slip, "⇄", "Slip", "スリップ", "Y"),
        ] {
            let clicked = ui
                .selectable_label(state.tool == tool, icon)
                .on_hover_text(format!("{} ({})", t(state.language, en, jp), key))
                .clicked();
            if clicked {
                state.tool = tool;
            }
        }
        ui.separator();
        if ui
            .small_button("T+")
            .on_hover_text(t(
                state.language,
                "Add a title at the playhead",
                "再生ヘッド位置にタイトルを追加",
            ))
            .clicked()
        {
            state.add_title_at_playhead();
        }
        ui.separator();
        for (label, mode, en, jp) in [
            (
                "INS",
                EditorEditMode::Insert,
                "Insert selected media at playhead (F9)",
                "選択メディアを再生ヘッドへ挿入 (F9)",
            ),
            (
                "OVR",
                EditorEditMode::Overwrite,
                "Overwrite at playhead (F10)",
                "再生ヘッド位置を上書き (F10)",
            ),
            (
                "REP",
                EditorEditMode::Replace,
                "Replace selected clip (F11)",
                "選択クリップを置換 (F11)",
            ),
        ] {
            if ui
                .small_button(label)
                .on_hover_text(t(state.language, en, jp))
                .clicked()
            {
                let _ = state.edit_selected_at_playhead(mode);
            }
        }
        ui.separator();
        if toggle_toolbar_button(
            ui,
            &mut state.snapping,
            "⌁",
            state.language,
            "Snapping (N)",
            "スナップ (N)",
        ) {
            state.mark_durable_edit();
        }
        if toggle_toolbar_button(
            ui,
            &mut state.linked_selection,
            "⛓",
            state.language,
            "Linked selection",
            "リンク選択",
        ) {
            state.mark_durable_edit();
        }
        if toggle_toolbar_button(
            ui,
            &mut state.position_lock,
            "▣",
            state.language,
            "Position lock",
            "位置ロック",
        ) {
            state.mark_durable_edit();
        }
        ui.menu_button("◆", |ui| {
            ui.label(t(state.language, "Timeline marker", "タイムラインマーカー"));
            marker_palette(ui, state.language, |color| state.add_marker(color));
            if ui
                .button(t(
                    state.language,
                    "Clear at playhead",
                    "再生ヘッドのマーカーを消去",
                ))
                .clicked()
            {
                state.clear_markers_at_playhead();
                ui.close();
            }
        })
        .response
        .on_hover_text(t(
            state.language,
            "Add or clear a colored timeline marker",
            "色付きタイムラインマーカーを追加または消去",
        ));
        ui.menu_button("⚑", |ui| {
            ui.label(t(state.language, "Source flag", "ソースフラグ"));
            marker_palette(ui, state.language, |color| state.set_selected_flag(color));
            if ui
                .button(t(state.language, "Clear selected flag", "選択フラグを消去"))
                .clicked()
            {
                state.clear_selected_flag();
                ui.close();
            }
        })
        .response
        .on_hover_text(t(
            state.language,
            "Flag selected timeline clip",
            "選択タイムラインクリップにフラグ",
        ));
        ui.menu_button("▤", |ui| {
            if ui
                .checkbox(
                    &mut state.show_video_thumbnails,
                    t(state.language, "Video thumbnails", "ビデオサムネイル"),
                )
                .changed()
            {
                state.mark_durable_edit();
            }
            if ui
                .checkbox(
                    &mut state.show_audio_waveforms,
                    t(state.language, "Audio waveforms", "オーディオ波形"),
                )
                .changed()
            {
                state.mark_durable_edit();
            }
            ui.separator();
            for (density, en, jp) in [
                (
                    TimelineTrackDensity::Compact,
                    "Compact tracks",
                    "コンパクトトラック",
                ),
                (
                    TimelineTrackDensity::Normal,
                    "Normal tracks",
                    "標準トラック",
                ),
                (
                    TimelineTrackDensity::Large,
                    "Large tracks",
                    "大きいトラック",
                ),
            ] {
                if ui
                    .selectable_label(state.track_density == density, t(state.language, en, jp))
                    .clicked()
                {
                    state.set_track_density(density);
                }
            }
        })
        .response
        .on_hover_text(t(state.language, "Timeline view", "タイムライン表示"));
        ui.menu_button("⌕", |ui| {
            if ui
                .button(t(state.language, "Full Extent", "全体表示"))
                .clicked()
            {
                state.set_full_extent_zoom();
                ui.close();
            }
            if ui.button(t(state.language, "Detail", "詳細表示")).clicked() {
                state.set_detail_zoom();
                ui.close();
            }
            if ui.button(t(state.language, "Custom", "カスタム")).clicked() {
                state.set_custom_timeline_view();
                ui.close();
            }
        })
        .response
        .on_hover_text(t(
            state.language,
            "Timeline zoom: Full Extent, Detail, or Custom",
            "タイムラインズーム: 全体・詳細・カスタム",
        ));
    });
}

fn toggle_toolbar_button(
    ui: &mut Ui,
    value: &mut bool,
    icon: &str,
    language: Language,
    en: &str,
    jp: &str,
) -> bool {
    if ui
        .selectable_label(*value, icon)
        .on_hover_text(t(language, en, jp))
        .clicked()
    {
        *value = !*value;
        true
    } else {
        false
    }
}

fn marker_palette(ui: &mut Ui, language: Language, mut select: impl FnMut(u8)) {
    ui.horizontal(|ui| {
        for index in 0..MARKER_COLORS.len() {
            let button = egui::Button::new("●").fill(marker_color(index as u8));
            if ui
                .add(button)
                .on_hover_text(t(language, "Choose color", "色を選択"))
                .clicked()
            {
                select(index as u8);
            }
        }
    });
}

/// Returns the time range represented by the horizontal navigator.
///
/// The navigator is intentionally wider than the visible view: it covers all placed media,
/// the current playhead/view (including a manually panned empty region), and practical forward
/// headroom for continuing an edit. The timeline view itself remains the single source of truth.
fn timeline_navigator_extent(state: &EditorState) -> Tick {
    let content_end = state
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .map(|clip| clip.end().0)
        .max()
        .unwrap_or(0);
    let visible_end = state
        .timeline_view_start
        .0
        .saturating_add(state.timeline_view_span.0.max(1));
    let frontier = content_end.max(state.playhead.0).max(visible_end);
    let reference_span = content_end.max(state.timeline_view_span.0).max(1_000_000);
    let headroom = (reference_span / 4).clamp(
        TIMELINE_NAVIGATOR_MIN_HEADROOM,
        TIMELINE_NAVIGATOR_MAX_HEADROOM,
    );
    Tick(frontier.saturating_add(headroom))
}

/// Calculates the start tick when the navigator track is clicked or dragged at `fraction`.
/// Centering the viewport at the pointer makes track clicks useful without a separate scroll
/// gesture and also keeps thumb drags responsive at either end of the range.
fn timeline_navigator_start_at_fraction(fraction: f32, extent: Tick, visible_span: Tick) -> Tick {
    let span = visible_span.0.max(1);
    let maximum_start = extent.0.saturating_sub(span).max(0);
    let pointer_tick = (extent.0 as f64 * fraction.clamp(0.0, 1.0) as f64).round() as i64;
    Tick(
        pointer_tick
            .saturating_sub(span / 2)
            .clamp(0, maximum_start),
    )
}

/// A navigator for timeline panning, separate from the Adobe-style two-handle zoom bar below.
/// It is shared by Edit and Undertow because both workspaces present the same timeline view.
fn timeline_pan_bar(ui: &mut Ui, state: &mut EditorState) {
    let desired = Vec2::new(ui.available_width(), 22.0);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let painter = ui.painter();
    let track = Rect::from_center_size(
        rect.center(),
        Vec2::new((rect.width() - 24.0).max(1.0), 7.0),
    );
    let extent = timeline_navigator_extent(state);
    let visible_span = state.timeline_view_span.0.max(1);
    let maximum_start = extent.0.saturating_sub(visible_span).max(0);
    let normalized_start = if maximum_start == 0 {
        0.0
    } else {
        (state.timeline_view_start.0 as f64 / maximum_start as f64).clamp(0.0, 1.0) as f32
    };
    let logical_thumb_width = track.width() * visible_span as f32 / extent.0.max(1) as f32;
    let thumb_width = logical_thumb_width
        .max(TIMELINE_NAVIGATOR_MIN_THUMB_WIDTH)
        .min(track.width());
    let thumb_left = track.left() + (track.width() - thumb_width) * normalized_start;
    let thumb = Rect::from_min_size(
        Pos2::new(thumb_left, track.top() - 2.0),
        Vec2::new(thumb_width, track.height() + 4.0),
    );

    painter.rect_filled(track, 3.5, Color32::from_rgb(35, 48, 60));
    painter.rect_stroke(
        track,
        3.5,
        Stroke::new(1.0, Color32::from_rgb(56, 72, 85)),
        StrokeKind::Inside,
    );
    let thumb_color = if response.dragged() {
        Color32::from_rgb(55, 142, 183)
    } else if response.hovered() {
        Color32::from_rgb(44, 116, 153)
    } else {
        Color32::from_rgb(37, 89, 119)
    };
    painter.rect_filled(thumb, 4.0, thumb_color);
    painter.rect_stroke(
        thumb,
        4.0,
        Stroke::new(1.0, Color32::from_rgb(125, 204, 235)),
        StrokeKind::Inside,
    );

    if (response.dragged() || response.clicked())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let fraction = (pointer.x - track.left()) / track.width().max(1.0);
        let target = timeline_navigator_start_at_fraction(fraction, extent, Tick(visible_span));
        state.pan_timeline_view(Tick(target.0.saturating_sub(state.timeline_view_start.0)));
        ui.ctx().request_repaint();
    }
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(if response.dragged() {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        });
    }
    response.on_hover_text(t(
        state.language,
        "Timeline navigator: drag the blue view to pan, or click the track to center the view.",
        "タイムラインナビゲーター: 青い表示範囲をドラッグしてパンし、トラックをクリックして表示を中央にします。",
    ));
}

fn zoom_bar(ui: &mut Ui, state: &mut EditorState) {
    let desired = Vec2::new(ui.available_width(), 24.0);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let painter = ui.painter();
    let track = Rect::from_center_size(
        rect.center(),
        Vec2::new((rect.width() - 32.0).max(1.0), 3.0),
    );
    painter.rect_filled(track, 2.0, Color32::from_rgb(53, 68, 81));
    let left_x = track.left() + track.width() * state.zoom_left;
    let right_x = track.left() + track.width() * state.zoom_right;
    painter.line_segment(
        [
            Pos2::new(left_x, track.center().y),
            Pos2::new(right_x, track.center().y),
        ],
        Stroke::new(3.0, Color32::from_rgb(76, 157, 196)),
    );
    for x in [left_x, right_x] {
        painter.circle_filled(
            Pos2::new(x, track.center().y),
            6.0,
            Color32::from_rgb(18, 27, 35),
        );
        painter.circle_stroke(
            Pos2::new(x, track.center().y),
            6.0,
            Stroke::new(1.5, Color32::from_rgb(171, 213, 233)),
        );
    }
    if response.dragged()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let normalized = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
        if (pointer.x - left_x).abs() <= (pointer.x - right_x).abs() {
            state.set_zoom_handles(normalized, state.zoom_right);
        } else {
            state.set_zoom_handles(state.zoom_left, normalized);
        }
    }
    response.on_hover_text(t(
        state.language,
        "Drag the two handles apart for a wider timeline view; together for a closer view.",
        "ハンドルを離すとタイムラインを広く表示し、近づけると拡大表示します。",
    ));
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Untitled media")
        .to_owned()
}
fn kind_icon(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "▣",
        MediaKind::Audio => "♪",
        MediaKind::Image => "▧",
        MediaKind::Unknown => "?",
    }
}
fn kind_name(language: Language, kind: MediaKind) -> &'static str {
    match (language, kind) {
        (Language::English, MediaKind::Video) => "Video",
        (Language::English, MediaKind::Audio) => "Audio",
        (Language::English, MediaKind::Image) => "Image",
        (Language::English, MediaKind::Unknown) => "Unknown",
        (Language::Japanese, MediaKind::Video) => "ビデオ",
        (Language::Japanese, MediaKind::Audio) => "オーディオ",
        (Language::Japanese, MediaKind::Image) => "画像",
        (Language::Japanese, MediaKind::Unknown) => "不明",
    }
}
fn t(language: Language, english: &str, japanese: &str) -> String {
    match language {
        Language::English => english,
        Language::Japanese => japanese,
    }
    .into()
}
fn menu_text(language: Language, english: &str, japanese: &str) -> String {
    t(language, english, japanese)
}
fn format_seconds(seconds: f32) -> String {
    format!("{:02}:{:02}", (seconds / 60.0) as u32, seconds as u32 % 60)
}

pub fn format_timecode_at_frame_rate(tick: Tick, frame_rate: ProjectFrameRate) -> String {
    let mut output = String::with_capacity(11);
    write_timecode(&mut output, tick, frame_rate);
    output
}

fn write_timecode(output: &mut String, tick: Tick, frame_rate: ProjectFrameRate) {
    let total_frames = frame_rate.frame_index_at_tick(tick);
    let frames_per_second = frame_rate.display_frames_per_second();
    let frames = total_frames % frames_per_second;
    let total_seconds = total_frames / frames_per_second;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3_600;
    write!(output, "{hours:02}:{minutes:02}:{seconds:02}:{frames:02}")
        .expect("writing to a String cannot fail");
}

fn fit_aspect(bounds: Rect, aspect: f32) -> Rect {
    let aspect = aspect.max(0.001);
    let size = if bounds.width() / bounds.height().max(1.0) > aspect {
        Vec2::new(bounds.height() * aspect, bounds.height())
    } else {
        Vec2::new(bounds.width(), bounds.width() / aspect)
    };
    Rect::from_center_size(bounds.center(), size)
}

/// Resolves the physical viewer raster used by Full preview. Quantization stabilizes cache keys
/// while preserving the requested display scale; only the 8K allocation guard can reduce it.
pub fn quantize_monitor_size(
    width_points: f32,
    height_points: f32,
    pixels_per_point: f32,
) -> (u32, u32) {
    let sanitize_points = |value: f32| {
        if value.is_finite() {
            value.max(MONITOR_SIZE_QUANTUM as f32)
        } else {
            MONITOR_SIZE_QUANTUM as f32
        }
    };
    let pixel_scale = if pixels_per_point.is_finite() {
        pixels_per_point.clamp(0.25, 8.0)
    } else {
        1.0
    };
    let available_w = sanitize_points(width_points) * pixel_scale;
    let available_h = sanitize_points(height_points) * pixel_scale;
    let scale = (MAX_MONITOR_DIMENSION as f32 / available_w)
        .min(MAX_MONITOR_DIMENSION as f32 / available_h)
        .min(1.0);
    let width = (available_w * scale).floor() as u32;
    let height = (available_h * scale).floor() as u32;
    let quantize = |value: u32| {
        (value / MONITOR_SIZE_QUANTUM)
            .max(1)
            .saturating_mul(MONITOR_SIZE_QUANTUM)
    };
    (quantize(width), quantize(height))
}

fn scale_monitor_size(size: (u32, u32), quality: PreviewQuality) -> (u32, u32) {
    let requested_divisor = quality.divisor();
    // If an unusually tiny viewer axis would fall below the decoder floor, reduce both axes by
    // the same effective divisor. Clamping axes independently would distort the picture.
    let effective_divisor = requested_divisor
        .min((size.0 / MONITOR_SIZE_QUANTUM).max(1))
        .min((size.1 / MONITOR_SIZE_QUANTUM).max(1));
    (
        (size.0 / effective_divisor).max(MONITOR_SIZE_QUANTUM),
        (size.1 / effective_divisor).max(MONITOR_SIZE_QUANTUM),
    )
}

fn timeline_tick_at(x: f32, content: Rect, view_start: Tick, visible_ticks: f32) -> Tick {
    let normalized = ((x - content.left()) / content.width().max(1.0)).clamp(0.0, 1.0);
    Tick(
        view_start
            .0
            .saturating_add((normalized * visible_ticks).round() as i64),
    )
}

/// Snaps horizontal structural edits to nearby clip boundaries or the playhead in screen space.
fn snap_timeline_tick(
    state: &EditorState,
    requested: Tick,
    visible_ticks: f32,
    width: f32,
    exclude_clip: Option<ClipId>,
) -> Tick {
    nearest_snap_delta(state, requested, visible_ticks, width, exclude_clip)
        .map(|delta| Tick(requested.0.saturating_add(delta)))
        .unwrap_or(requested)
}

fn nearest_snap_delta(
    state: &EditorState,
    requested: Tick,
    visible_ticks: f32,
    width: f32,
    exclude_clip: Option<ClipId>,
) -> Option<i64> {
    if !state.snapping {
        return None;
    }
    let threshold = (visible_ticks / width.max(1.0) * 8.0).round() as i64;
    let excluded = exclude_clip.and_then(|clip_id| state.timeline.clip(clip_id));
    let mut nearest = None;
    let mut consider = |candidate: Tick| {
        let delta = candidate.0.saturating_sub(requested.0);
        if delta.unsigned_abs() <= threshold as u64
            && nearest.is_none_or(|current: i64| delta.unsigned_abs() < current.unsigned_abs())
        {
            nearest = Some(delta);
        }
    };
    let lower_tick = Tick(requested.0.saturating_sub(threshold));
    let upper_tick = Tick(requested.0.saturating_add(threshold));
    for track in &state.timeline.tracks {
        let first_start = track.clips.partition_point(|clip| clip.start < lower_tick);
        let end_start = track.clips.partition_point(|clip| clip.start <= upper_tick);
        for clip in &track.clips[first_start..end_start] {
            let is_excluded = excluded.is_some_and(|target| {
                clip.id == target.id
                    || (state.linked_selection
                        && target.link_id.is_some_and(|link_id| {
                            clip.link_id == Some(link_id)
                                && clip.start == target.start
                                && clip.duration == target.duration
                        }))
            });
            if !is_excluded {
                consider(clip.start);
            }
        }
        let first_end = track.clips.partition_point(|clip| clip.end() < lower_tick);
        let end_end = track.clips.partition_point(|clip| clip.end() <= upper_tick);
        for clip in &track.clips[first_end..end_end] {
            let is_excluded = excluded.is_some_and(|target| {
                clip.id == target.id
                    || (state.linked_selection
                        && target.link_id.is_some_and(|link_id| {
                            clip.link_id == Some(link_id)
                                && clip.start == target.start
                                && clip.duration == target.duration
                        }))
            });
            if !is_excluded {
                consider(clip.end());
            }
        }
    }
    for marker in &state.markers {
        consider(marker.tick);
    }
    consider(state.playhead);
    nearest
}

/// Moves can snap either the incoming or outgoing clip boundary. Return the adjustment that
/// changes the requested start by the smallest amount.
fn snap_move_start(
    state: &EditorState,
    requested_start: Tick,
    duration: Tick,
    visible_ticks: f32,
    width: f32,
    clip_id: ClipId,
) -> Tick {
    let start_delta =
        nearest_snap_delta(state, requested_start, visible_ticks, width, Some(clip_id));
    let requested_end = Tick(requested_start.0.saturating_add(duration.0));
    let end_delta = nearest_snap_delta(state, requested_end, visible_ticks, width, Some(clip_id));
    [start_delta, end_delta]
        .into_iter()
        .flatten()
        .min_by_key(|delta| delta.abs())
        .map(|delta| Tick(requested_start.0.saturating_add(delta)))
        .unwrap_or(requested_start)
}

fn timeline_is_empty(timeline: &Timeline) -> bool {
    timeline.tracks.iter().all(|track| track.clips.is_empty())
}

fn drop_pointer_position(
    latest: Option<Pos2>,
    hovered: Option<Pos2>,
    interaction: Option<Pos2>,
) -> Option<Pos2> {
    latest.or(hovered).or(interaction)
}

fn media_drop_start(timeline: &Timeline, requested_start: Tick) -> Tick {
    if timeline_is_empty(timeline) {
        Tick(0)
    } else {
        requested_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_timeline::Clip;

    #[derive(Debug, PartialEq)]
    enum ViewerEvent {
        Layer {
            slot: usize,
            clip: ClipId,
            opacity: f32,
        },
        BlackMatte(f32),
    }

    #[derive(Default)]
    struct RecordingViewerCanvas {
        events: Vec<ViewerEvent>,
    }

    impl ViewerCanvas for RecordingViewerCanvas {
        fn begin(&mut self, _: &mut Ui, _: Rect, _: PixelSize) {
            self.events.clear();
        }

        fn layer(
            &mut self,
            layer: usize,
            _: MonitorFrame,
            _: Rect,
            quad: CompositeQuad,
            _: EvaluatedVideoEffectStack,
        ) {
            self.events.push(ViewerEvent::Layer {
                slot: layer,
                clip: quad.clip_id,
                opacity: quad.opacity,
            });
        }

        fn black_matte(&mut self, opacity: f32) {
            self.events.push(ViewerEvent::BlackMatte(opacity));
        }
    }

    #[test]
    fn imports_are_deduplicated_and_classified() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([
            PathBuf::from("A.MP4"),
            PathBuf::from("mix.wav"),
            PathBuf::from("A.MP4"),
        ]);
        assert_eq!(editor.media.len(), 2);
        assert_eq!(editor.media[0].kind, MediaKind::Video);
        assert_eq!(editor.media[1].kind, MediaKind::Audio);
        assert_eq!(editor.selected_media, Some(1));
    }

    #[test]
    fn media_pool_placement_controls_accept_still_images() {
        assert!(media_kind_can_place(MediaKind::Video));
        assert!(media_kind_can_place(MediaKind::Audio));
        assert!(media_kind_can_place(MediaKind::Image));
        assert!(!media_kind_can_place(MediaKind::Unknown));
    }

    #[test]
    fn offline_media_uses_magenta_palette_until_probe_recovers() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("missing.mp4")]);
        editor.set_media_error(1, "file not found");
        assert!(editor.media_errors.contains_key(&1));
        assert!(editor.media_is_offline(1));
        assert_eq!(
            timeline_clip_palette(TrackKind::Video, 1, true),
            (
                Color32::from_rgb(164, 35, 137),
                Color32::from_rgb(255, 126, 224)
            )
        );

        editor.set_media_metadata(1, MediaMetadata::default());
        assert!(!editor.media_errors.contains_key(&1));
        assert!(!editor.media_is_offline(1));
    }

    #[test]
    fn media_identity_selects_a_stable_dark_video_and_audio_palette() {
        let first_video = timeline_clip_palette(TrackKind::Video, 1, false);
        assert_eq!(
            first_video,
            timeline_clip_palette(TrackKind::Video, 1, false)
        );
        assert_ne!(
            first_video,
            timeline_clip_palette(TrackKind::Audio, 1, false),
            "linked A/V bars keep their track identity"
        );

        let mut video_fills = HashSet::new();
        for media_id in 1..=32 {
            let video = timeline_clip_palette(TrackKind::Video, media_id, false);
            let audio = timeline_clip_palette(TrackKind::Audio, media_id, false);
            assert!(video.0.r() < video.1.r() || video.0.g() < video.1.g());
            assert!(audio.0.g() < audio.1.g());
            video_fills.insert(video.0.to_array());
        }
        assert_eq!(video_fills.len(), VIDEO_CLIP_PALETTES.len());

        assert_eq!(
            timeline_clip_palette(TrackKind::Video, 1, true),
            timeline_clip_palette(TrackKind::Video, 99, true),
            "offline media remains unambiguously magenta"
        );
    }

    #[test]
    fn metadata_selection_retains_path_and_kind() {
        let mut editor = EditorState::new(Language::Japanese, "作品");
        editor.add_media_paths([PathBuf::from("still.exr")]);
        let item = editor.selected().unwrap();
        assert_eq!(item.path, PathBuf::from("still.exr"));
        assert_eq!(item.kind, MediaKind::Image);
        assert_eq!(kind_name(editor.language, item.kind), "画像");
    }

    #[test]
    fn actions_are_one_shot() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.emit(EditorAction::ChooseMediaFiles);
        assert_eq!(editor.take_action(), Some(EditorAction::ChooseMediaFiles));
        assert_eq!(editor.take_action(), None);
    }

    #[test]
    fn undertow_switches_views_without_copying_or_dirtying_the_project() {
        let mut editor = EditorState::new(Language::English, "Shared audio project");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .expect("linked audio clip")
            .clone();
        editor.timeline.set_audio_gain(audio.id, 5.0).unwrap();
        editor
            .timeline
            .set_fade_duration(audio.id, FadeEdge::In, Tick(750_000))
            .unwrap();
        let before = editor.snapshot();
        let generation = editor.durable_generation();

        editor.set_workspace(EditorWorkspace::Undertow);
        assert_eq!(editor.workspace, EditorWorkspace::Undertow);
        assert_eq!(editor.undertow_track, Some(audio.track_id));
        assert_eq!(editor.timeline.clip(audio.id).unwrap().gain_db, 5.0);
        editor.set_workspace(EditorWorkspace::Edit);

        assert_eq!(editor.snapshot(), before);
        assert_eq!(editor.durable_generation(), generation);
    }

    #[test]
    fn kraken_upscale_stays_gated_until_capable() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.set_workspace(EditorWorkspace::KrakenUpscale);
        assert_eq!(editor.workspace, EditorWorkspace::Edit);
        editor.set_kraken_upscale_capability(true, "NVIDIA RTX VSR ready");
        editor.set_workspace(EditorWorkspace::KrakenUpscale);
        assert_eq!(editor.workspace, EditorWorkspace::KrakenUpscale);
        editor.set_kraken_upscale_capability(false, "No NVIDIA GPU");
        assert_eq!(editor.workspace, EditorWorkspace::Edit);
    }

    #[test]
    fn undertow_presentation_expands_only_the_focused_audio_track() {
        let focus = TrackId(7);
        let presentation = TimelinePresentation {
            show_tool_row: false,
            audio_focus: Some(focus),
        };
        assert_eq!(
            presented_track_height(TrackKind::Audio, focus, 92.0, presentation),
            160.0
        );
        assert_eq!(
            presented_track_height(TrackKind::Audio, TrackId(8), 120.0, presentation),
            MIN_TIMELINE_TRACK_HEIGHT
        );
        assert_eq!(
            presented_track_height(TrackKind::Video, TrackId(1), 120.0, presentation),
            MIN_TIMELINE_TRACK_HEIGHT
        );
        assert_eq!(
            presented_track_height(
                TrackKind::Audio,
                focus,
                92.0,
                TimelinePresentation {
                    show_tool_row: true,
                    audio_focus: None,
                }
            ),
            92.0
        );
    }

    #[test]
    fn undertow_track_selector_scrolls_the_focused_audio_lane_into_view() {
        let mut editor = EditorState::new(Language::English, "Track focus");
        let third_audio = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .nth(2)
            .unwrap()
            .id;
        editor.focus_undertow_track(third_audio);

        assert_eq!(editor.undertow_track, Some(third_audio));
        assert_eq!(editor.timeline_scroll_y, MIN_TIMELINE_TRACK_HEIGHT * 5.0);
    }

    #[test]
    fn fade_shading_reaches_both_outer_edges_and_follows_the_envelope() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0));
        for curve in [-1.0, 1.0] {
            let fade = nle_timeline::Fade {
                duration: Tick(50),
                curve,
            };
            for edge in [FadeEdge::In, FadeEdge::Out] {
                let geometry = fade_control_geometry(rect, Tick(100), fade, edge);
                let bands = fade_shade_bands(rect, geometry);
                assert_eq!(bands.len(), 21);
                assert_eq!(bands.first().unwrap().1, geometry.outer_endpoint);
                assert_eq!(bands.last().unwrap().1, geometry.full_endpoint);
                assert!(bands.iter().all(|(top, _)| top.y == rect.top()));
                assert_eq!(bands[10].1, geometry.curve_point);
            }
        }
    }

    #[test]
    fn zoom_handles_stay_ordered_with_minimum_gap() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.set_zoom_handles(0.85, 0.2);
        assert!(editor.zoom_left >= 0.0);
        assert!(editor.zoom_right <= 1.0);
        assert!(editor.zoom_right - editor.zoom_left >= 0.01 - f32::EPSILON);
        editor.set_zoom_handles(-5.0, 8.0);
        assert_eq!(editor.zoom_left, 0.0);
        assert_eq!(editor.zoom_right, 1.0);
    }

    #[test]
    fn workspace_reset_restores_panels_scroll_density_and_track_heights_once() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.media_pool_width = 410.0;
        editor.analysis_width = 520.0;
        editor.timeline_height = 610.0;
        editor.timeline_height_is_default = false;
        editor.timeline_scroll_y = 75.0;
        editor.set_track_density(TimelineTrackDensity::Large);
        let generation = editor.durable_generation();

        editor.reset_workspace_layout();

        assert_eq!(editor.media_pool_width, DEFAULT_MEDIA_POOL_WIDTH);
        assert_eq!(editor.analysis_width, DEFAULT_RIGHT_SIDEBAR_WIDTH);
        assert_eq!(editor.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(editor.timeline_height_is_default);
        assert_eq!(editor.timeline_scroll_y, 0.0);
        assert_eq!(editor.track_density, TimelineTrackDensity::Normal);
        assert!(editor.track_heights.values().all(|height| *height == 64.0));
        assert_eq!(editor.durable_generation(), generation + 1);

        editor.reset_workspace_layout();
        assert_eq!(editor.durable_generation(), generation + 1);
    }

    #[test]
    fn reopen_upgrades_only_known_default_timeline_heights() {
        let editor = EditorState::new(Language::English, "Layout migration");
        let mut legacy = editor.snapshot();
        legacy.view.timeline_height = LEGACY_DEFAULT_TIMELINE_HEIGHT;
        legacy.view.timeline_height_is_default = None;
        let restored = EditorState::restore(Language::English, "Layout migration", legacy)
            .expect("restore legacy layout");
        assert_eq!(restored.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(restored.timeline_height_is_default);

        let mut previous_layout = editor.snapshot();
        previous_layout.view.timeline_height = PREVIOUS_LAYOUT_DEFAULT_TIMELINE_HEIGHT;
        previous_layout.view.timeline_height_is_default = None;
        let restored = EditorState::restore(Language::English, "Layout migration", previous_layout)
            .expect("restore previous layout default");
        assert_eq!(restored.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(restored.timeline_height_is_default);

        let mut previous = editor.snapshot();
        previous.view.timeline_height = PREVIOUS_DEFAULT_TIMELINE_HEIGHT;
        previous.view.timeline_height_is_default = None;
        let restored = EditorState::restore(Language::English, "Layout migration", previous)
            .expect("restore previous default layout");
        assert_eq!(restored.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(restored.timeline_height_is_default);

        let mut former = editor.snapshot();
        former.view.timeline_height = FORMER_DEFAULT_TIMELINE_HEIGHT;
        former.view.timeline_height_is_default = None;
        let restored = EditorState::restore(Language::English, "Layout migration", former)
            .expect("restore former default layout");
        assert_eq!(restored.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(restored.timeline_height_is_default);

        let mut pre_responsive_json = serde_json::to_value(editor.snapshot()).unwrap();
        let view = pre_responsive_json
            .get_mut("view")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        view.remove("timeline_height_is_default");
        view.insert(
            "timeline_height".to_owned(),
            serde_json::json!(FORMER_DEFAULT_TIMELINE_HEIGHT),
        );
        let restored = EditorState::restore(
            Language::English,
            "Layout migration",
            serde_json::from_value(pre_responsive_json).unwrap(),
        )
        .expect("restore pre-responsive layout");
        assert!(restored.timeline_height_is_default);

        let mut old_custom_json = serde_json::to_value(editor.snapshot()).unwrap();
        let view = old_custom_json
            .get_mut("view")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        view.remove("timeline_height_is_default");
        view.insert("timeline_height".to_owned(), serde_json::json!(440.0));
        let restored = EditorState::restore(
            Language::English,
            "Layout migration",
            serde_json::from_value(old_custom_json).unwrap(),
        )
        .expect("restore pre-marker custom layout");
        assert_eq!(restored.timeline_height, 440.0);
        assert!(!restored.timeline_height_is_default);

        let mut exact_default_but_explicitly_custom = editor.snapshot();
        exact_default_but_explicitly_custom.view.timeline_height = DEFAULT_TIMELINE_HEIGHT;
        exact_default_but_explicitly_custom
            .view
            .timeline_height_is_default = Some(false);
        let restored = EditorState::restore(
            Language::English,
            "Layout migration",
            exact_default_but_explicitly_custom,
        )
        .expect("restore explicit custom layout");
        assert_eq!(restored.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(!restored.timeline_height_is_default);

        let mut customized = editor.snapshot();
        customized.view.timeline_height = 440.0;
        customized.view.timeline_height_is_default = Some(false);
        let restored = EditorState::restore(Language::English, "Layout migration", customized)
            .expect("restore custom layout");
        assert_eq!(restored.timeline_height, 440.0);
        assert!(!restored.timeline_height_is_default);
    }

    #[test]
    fn clip_context_target_is_runtime_only_and_split_requires_an_interior_playhead() {
        let mut editor = EditorState::new(Language::English, "Clip context menu");
        editor.add_media_paths([PathBuf::from("context.mp4")]);
        assert!(editor.insert_media_at(1, Tick(100)));
        let clip_id = editor
            .selected_timeline_clip
            .expect("inserted clip selected");
        let clip = editor.timeline.clip(clip_id).expect("inserted clip exists");

        assert!(!clip_can_split_at_playhead(clip, clip.start));
        assert!(clip_can_split_at_playhead(clip, Tick(clip.start.0 + 1)));
        assert!(!clip_can_split_at_playhead(clip, clip.end()));

        let snapshot = editor.snapshot();
        editor.timeline_context_clip = Some(clip_id);
        assert_eq!(editor.snapshot(), snapshot);
    }

    #[test]
    fn responsive_default_timeline_uses_requested_share_or_a_complete_small_window_minimum() {
        let mut previous = 0.0;
        for remaining in [650.0, 1_010.0, 1_330.0, 2_090.0] {
            let timeline = responsive_default_timeline_height(remaining);
            let viewer = (remaining - timeline - SPLITTER_THICKNESS).max(180.0);
            assert!(
                timeline <= viewer * 1.5,
                "{remaining}px resolved to an oversized {timeline}px timeline and {viewer}px viewer"
            );
            let requested = remaining * DEFAULT_TIMELINE_HEIGHT_FRACTION;
            if requested >= MIN_COMPLETE_TIMELINE_PANEL_HEIGHT {
                assert!(
                    (timeline / remaining - DEFAULT_TIMELINE_HEIGHT_FRACTION).abs() < 0.01,
                    "{remaining}px default should preserve the requested timeline share: {timeline}px timeline"
                );
            } else {
                assert_eq!(timeline, MIN_COMPLETE_TIMELINE_PANEL_HEIGHT);
            }
            assert!(timeline > previous);
            previous = timeline;
        }
    }

    #[test]
    fn toolbar_view_defaults_restore_from_pre_toolbar_snapshot_json() {
        let editor = EditorState::new(Language::English, "Test");
        let mut value = serde_json::to_value(editor.snapshot()).unwrap();
        let view = value
            .get_mut("view")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        for field in [
            "timeline_view_start",
            "timeline_view_span",
            "snapping",
            "linked_selection",
            "position_lock",
            "show_video_thumbnails",
            "show_audio_waveforms",
            "track_density",
            "markers",
            "flags",
        ] {
            view.remove(field);
        }
        let snapshot: EditorProjectSnapshot = serde_json::from_value(value).unwrap();
        let restored = EditorState::restore(Language::English, "Test", snapshot).unwrap();
        assert!(restored.snapping);
        assert!(restored.linked_selection);
        assert!(!restored.position_lock);
        assert!(restored.show_video_thumbnails);
        assert!(restored.show_audio_waveforms);
        assert!(restored.markers.is_empty());
        assert!(restored.flags.is_empty());
        assert_eq!(restored.timeline_view_start, Tick(0));
        assert_eq!(restored.timeline_view_span, legacy_zoom_span(0.08, 0.92));
    }

    #[test]
    fn snapping_uses_nearby_clip_boundaries_only_when_enabled() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let near_start = Tick(40_000);
        editor.snapping = false;
        assert_eq!(
            snap_timeline_tick(&editor, near_start, 15_000_000.0, 1_000.0, None),
            near_start
        );
        editor.snapping = true;
        assert_eq!(
            snap_timeline_tick(&editor, near_start, 15_000_000.0, 1_000.0, None),
            Tick(0)
        );
        assert_eq!(
            snap_timeline_tick(&editor, Tick(8_000_000), 15_000_000.0, 1_000.0, None,),
            Tick(8_000_000)
        );
        let clip_id = editor.selected_timeline_clip.unwrap();
        editor.set_playhead(Tick(8_000_000));
        assert_eq!(
            snap_timeline_tick(&editor, near_start, 15_000_000.0, 1_000.0, Some(clip_id),),
            near_start
        );
    }

    #[test]
    fn moving_clip_can_snap_its_far_edge_without_sticking_to_itself() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        let moving = editor.selected_timeline_clip.unwrap();
        assert!(editor.insert_media_at(2, Tick(20_000_000)));
        editor.set_playhead(Tick(9_000_000));
        editor.snapping = true;
        assert_eq!(
            snap_move_start(
                &editor,
                Tick(4_960_000),
                Tick(15_000_000),
                30_000_000.0,
                1_000.0,
                moving,
            ),
            Tick(5_000_000)
        );
    }

    #[test]
    fn markers_and_flags_are_durable_but_runtime_caches_remain_excluded() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_playhead(Tick(1_000_000));
        editor.add_marker(2);
        editor.set_selected_flag(3);
        let flagged_media = editor
            .timeline
            .clip(editor.selected_timeline_clip.unwrap())
            .unwrap()
            .media
            .0;
        let snapshot = editor.snapshot();
        assert_eq!(snapshot.view.markers.len(), 1);
        assert_eq!(snapshot.view.flags.len(), 1);
        assert_eq!(snapshot.view.flags[0].media_id, flagged_media);
        let restored = EditorState::restore(Language::English, "Test", snapshot).unwrap();
        assert_eq!(restored.markers.len(), 1);
        assert_eq!(restored.flags.len(), 1);
        assert!(restored.video_strips.is_empty());
    }

    #[test]
    fn legacy_clip_flag_migrates_to_source_flag_on_restore() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        let expected_media = editor.timeline.clip(clip_id).unwrap().media.0;
        let mut value = serde_json::to_value(editor.snapshot()).unwrap();
        value["view"]["flags"] = serde_json::json!([{"clip_id": clip_id.0, "color": 2}]);
        let snapshot: EditorProjectSnapshot = serde_json::from_value(value).unwrap();
        let restored = EditorState::restore(Language::English, "Test", snapshot).unwrap();
        assert_eq!(restored.flags[0].media_id, expected_media);
        let saved = serde_json::to_value(restored.snapshot()).unwrap();
        assert!(saved["view"]["flags"][0].get("media_id").is_some());
        assert!(saved["view"]["flags"][0].get("clip_id").is_none());
    }

    #[test]
    fn zoom_presets_fit_the_actual_timeline_and_offer_a_detail_view() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        assert!(editor.add_selected_to_timeline());
        editor.set_full_extent_zoom();
        assert!(editor.visible_time_seconds() >= 30.0);
        assert!(editor.visible_time_seconds() < 34.0);
        editor.set_detail_zoom();
        assert!((editor.visible_time_seconds() - 15.0).abs() < 0.001);
    }

    #[test]
    fn first_placement_fits_the_clip_but_later_placements_preserve_user_view() {
        let mut editor = EditorState::new(Language::English, "First placement");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);

        assert!(editor.insert_media_at(1, Tick(0)));
        assert_eq!(editor.timeline_view_start, Tick(0));
        assert_eq!(editor.timeline_view_span, Tick(16_000_000));

        editor.timeline_view_start = Tick(2_000_000);
        editor.timeline_view_span = Tick(8_000_000);
        assert!(editor.insert_media_at(2, Tick(20_000_000)));
        assert_eq!(editor.timeline_view_start, Tick(2_000_000));
        assert_eq!(editor.timeline_view_span, Tick(8_000_000));
    }

    #[test]
    fn position_lock_allows_replace_but_blocks_ripple_insert() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("replacement.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let selected = editor.selected_timeline_clip.unwrap();
        editor.selected_media = Some(2);
        editor.position_lock = true;
        assert!(editor.edit_selected_at_playhead(EditorEditMode::Replace));
        assert_eq!(editor.timeline.clip(selected).unwrap().media.0, 2);
        assert!(!editor.edit_selected_at_playhead(EditorEditMode::Insert));
    }

    #[test]
    fn linked_replace_rejects_audio_source_when_audio_half_is_selected() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("video.mp4"), PathBuf::from("audio.wav")]);
        assert!(editor.add_selected_to_timeline());
        let audio_half = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .clips[0]
            .id;
        editor.selected_timeline_clip = Some(audio_half);
        editor.selected_media = Some(2);
        assert!(!editor.edit_selected_at_playhead(EditorEditMode::Replace));
        assert!(editor.timeline.tracks.iter().all(|track| {
            track
                .clips
                .iter()
                .all(|clip| clip.media == TimelineMediaId(1))
        }));
    }

    #[test]
    fn view_range_maps_nonzero_origin_and_full_extent_is_not_capped() {
        let content = Rect::from_min_size(Pos2::new(100.0, 20.0), Vec2::new(400.0, 100.0));
        assert_eq!(
            timeline_tick_at(content.center().x, content, Tick(100_000_000), 20_000_000.0),
            Tick(110_000_000)
        );
        let rect = clip_rect_for(
            content,
            content,
            Tick(105_000_000),
            Tick(5_000_000),
            Tick(100_000_000),
            20_000_000.0,
        );
        assert_eq!(rect.left(), 200.0);
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("late.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        assert!(editor.insert_media_at(1, Tick(600_000_000)));
        editor.set_full_extent_zoom();
        assert!(editor.timeline_view_span.0 > 300_000_000);
        assert_eq!(editor.timeline_view_start, Tick(0));
    }

    #[test]
    fn detail_view_centers_a_late_playhead() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("late.mp4")]);
        assert!(editor.insert_media_at(1, Tick(600_000_000)));
        editor.set_playhead(Tick(600_000_000));
        editor.set_detail_zoom();
        assert_eq!(editor.timeline_view_span, Tick(15_000_000));
        assert_eq!(editor.timeline_view_start, Tick(592_500_000));
    }

    #[test]
    fn workspace_splitters_have_sensible_persistent_defaults() {
        let editor = EditorState::new(Language::English, "Test");
        assert!(editor.media_pool_width >= 190.0);
        assert!(editor.analysis_width >= 220.0);
        assert_eq!(editor.timeline_height, DEFAULT_TIMELINE_HEIGHT);
        assert!(editor.timeline_height > LEGACY_DEFAULT_TIMELINE_HEIGHT);
    }

    #[test]
    fn dense_track_layout_stays_usable_and_clamps_scroll() {
        let heights = vec![MIN_TIMELINE_TRACK_HEIGHT; 24];
        let (total_height, max_scroll, scroll) = track_layout(&heights, 180.0, 9_999.0);
        assert_eq!(total_height, MIN_TIMELINE_TRACK_HEIGHT * 24.0);
        assert!(max_scroll > 0.0);
        assert_eq!(scroll, max_scroll);
    }

    #[test]
    fn track_layout_does_not_scroll_when_all_rows_fit() {
        let (_, max_scroll, scroll) = track_layout(&[32.0, 32.0, 32.0], 180.0, 20.0);
        assert_eq!(max_scroll, 0.0);
        assert_eq!(scroll, 0.0);
    }

    #[test]
    fn variable_track_geometry_maps_hits_and_visible_rows() {
        let heights = [32.0, 96.0, 40.0];
        assert_eq!(track_row_at_y(&heights, 100.0, 20.0, 85.0), Some(0));
        assert_eq!(track_row_at_y(&heights, 100.0, 20.0, 120.0), Some(1));
        assert_eq!(visible_track_range(&heights, 100.0, 20.0, 110.0), 0..3);
    }

    #[test]
    fn track_heights_clamp_to_editable_bounds() {
        assert_eq!(clamp_track_height(-20.0), MIN_TIMELINE_TRACK_HEIGHT);
        assert_eq!(clamp_track_height(10_000.0), MAX_TIMELINE_TRACK_HEIGHT);
    }

    #[test]
    fn track_resize_grip_spans_the_timeline_without_covering_clip_controls() {
        let viewport = Rect::from_min_max(Pos2::new(10.0, 100.0), Pos2::new(410.0, 300.0));
        let row = Rect::from_min_max(Pos2::new(104.0, 140.0), Pos2::new(410.0, 180.0));
        let grip = track_resize_grip(row, viewport);

        assert_eq!(grip.left(), viewport.left());
        assert_eq!(grip.right(), viewport.right());
        assert!(grip.contains(Pos2::new(300.0, row.bottom() + 6.0)));
        assert!(!grip.contains(Pos2::new(300.0, row.bottom() - 3.0)));
    }

    #[test]
    fn dragging_the_full_width_track_boundary_resizes_and_persists_the_row() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Track resize gesture");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());

        timeline_input_frame(&context, &mut editor, Vec::new());
        let geometry = editor.timeline_drop_geometry.expect("timeline geometry");
        let resized_track = editor.timeline.tracks[0].id;
        let neighboring_track = editor.timeline.tracks[1].id;
        let initial_generation = editor.durable_generation();
        let boundary_y = geometry.content.top() + DEFAULT_TIMELINE_TRACK_HEIGHT;
        // Press inside the timeline canvas—not the track header—to prove the boundary really
        // remains draggable across its full width while a clip occupies this row.
        let press = Pos2::new(geometry.content.center().x, boundary_y + 3.0);
        let target = press + Vec2::new(0.0, 48.0);

        assert!(
            track_resize_grip(
                Rect::from_min_size(
                    geometry.content.left_top(),
                    Vec2::new(geometry.content.width(), DEFAULT_TIMELINE_TRACK_HEIGHT),
                ),
                Rect::from_min_max(
                    Pos2::new(geometry.rect.left(), geometry.content.top()),
                    geometry.rect.right_bottom(),
                ),
            )
            .contains(press)
        );
        drag_timeline_pointer(&context, &mut editor, press, target);

        let resized_height = editor
            .track_heights
            .get(&resized_track)
            .copied()
            .expect("resized track height");
        assert!(
            (resized_height - 80.0).abs() < 0.1,
            "height was {resized_height}"
        );
        assert!(!editor.track_heights.contains_key(&neighboring_track));
        assert!(editor.timeline_drag.is_none());
        assert!(editor.durable_generation() > initial_generation);

        let restored =
            EditorState::restore(Language::English, "Track resize gesture", editor.snapshot())
                .expect("restore resized track");
        assert_eq!(
            restored.track_heights.get(&resized_track),
            Some(&resized_height)
        );
        assert!(!restored.track_heights.contains_key(&neighboring_track));
    }

    #[test]
    fn timeline_clip_labels_reuse_retained_galleys_and_skip_dense_bars() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Retained labels");
        editor.add_media_paths([PathBuf::from("a-long-readable-media-name.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.timeline_view_span = Tick(15_000_000);

        timeline_input_frame(&context, &mut editor, Vec::new());
        let first = editor
            .timeline_label_galleys
            .first()
            .and_then(Option::as_ref)
            .expect("wide clip creates one retained label")
            .clone();
        timeline_input_frame(&context, &mut editor, Vec::new());
        let second = editor.timeline_label_galleys[0]
            .as_ref()
            .expect("retained label remains cached");
        assert!(Arc::ptr_eq(&first, second));

        editor.timeline_label_galleys.clear();
        editor.timeline_view_span = Tick(1_000_000_000);
        timeline_input_frame(&context, &mut editor, Vec::new());
        assert!(
            editor.timeline_label_galleys.is_empty(),
            "sub-label-width bars must not perform text layout"
        );
        assert!(!timeline_clip_label_is_visible(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(MIN_TIMELINE_CLIP_LABEL_WIDTH - 1.0, 64.0),
        )));
    }

    #[test]
    fn waveform_status_text_is_retained_once_and_skipped_for_dense_bars() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Retained waveform status");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.timeline_view_span = Tick(15_000_000);

        timeline_input_frame(&context, &mut editor, Vec::new());
        let first = editor
            .timeline_waveform_pending_galley
            .as_ref()
            .expect("wide pending waveform creates one retained status")
            .clone();
        timeline_input_frame(&context, &mut editor, Vec::new());
        assert!(Arc::ptr_eq(
            &first,
            editor
                .timeline_waveform_pending_galley
                .as_ref()
                .expect("pending status remains retained")
        ));

        editor.set_waveform_error(1, "probe failed");
        timeline_input_frame(&context, &mut editor, Vec::new());
        assert!(editor.timeline_waveform_failed_galley.is_some());

        editor.timeline_waveform_pending_galley = None;
        editor.timeline_waveform_failed_galley = None;
        editor
            .set_waveform(1, Tick(15_000_000), Vec::new())
            .unwrap();
        editor.timeline_view_span = Tick(1_000_000_000);
        timeline_input_frame(&context, &mut editor, Vec::new());
        assert!(editor.timeline_waveform_pending_galley.is_none());
        assert!(editor.timeline_waveform_failed_galley.is_none());
    }

    #[test]
    fn timeline_media_draw_slots_retain_runtime_assets_and_invalidate_on_changes() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Media draw slots");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor
            .set_waveform(1, Tick(15_000_000), vec![(-0.4, 0.6), (-0.8, 0.2)])
            .unwrap();
        editor.set_video_strip(
            1,
            9,
            egui::TextureId::Managed(9),
            VideoStripLayout {
                duration: Tick(15_000_000),
                frame_count: 4,
                columns: 2,
                rows: 2,
                frame_width: 160,
                frame_height: 90,
            },
        );
        editor.set_media_error(1, "offline");
        editor.set_selected_flag(3);
        assert!(editor.timeline_media_draw_slots_dirty);

        timeline_input_frame(&context, &mut editor, Vec::new());
        assert!(!editor.timeline_media_draw_slots_dirty);
        let slot = &editor.timeline_media_draw_slots[0];
        let first_waveform = slot.waveform.as_ref().unwrap().clone();
        assert!(slot.offline);
        assert!(!slot.waveform_failed);
        assert_eq!(slot.video_strip.unwrap().native_texture_id, 9);
        assert_eq!(slot.flag_color, Some(marker_color(3)));

        timeline_input_frame(&context, &mut editor, Vec::new());
        assert!(Arc::ptr_eq(
            &first_waveform,
            editor.timeline_media_draw_slots[0]
                .waveform
                .as_ref()
                .unwrap()
        ));
        assert!(!editor.timeline_media_draw_slots_dirty);

        editor.set_waveform_error(1, "waveform failed");
        editor.set_media_metadata(1, MediaMetadata::default());
        editor.clear_selected_flag();
        assert!(editor.timeline_media_draw_slots_dirty);
        timeline_input_frame(&context, &mut editor, Vec::new());
        let slot = &editor.timeline_media_draw_slots[0];
        assert!(slot.waveform.is_none());
        assert!(slot.waveform_failed);
        assert!(!slot.offline);
        assert!(slot.flag_color.is_none());
    }

    #[test]
    fn playhead_handle_is_a_wide_ruler_grab_and_maps_to_timeline_ticks() {
        let ruler = Rect::from_min_max(Pos2::new(100.0, 40.0), Pos2::new(500.0, 64.0));
        let handle = playhead_handle_rect(300.0, ruler);
        assert_eq!(handle.width(), PLAYHEAD_HANDLE_WIDTH);
        assert!(handle.contains(Pos2::new(293.0, 52.0)));
        assert!(!handle.contains(Pos2::new(291.0, 52.0)));

        let content = Rect::from_min_max(Pos2::new(100.0, 64.0), Pos2::new(500.0, 300.0));
        assert_eq!(
            timeline_tick_at(handle.center().x, content, Tick(0), 10_000_000.0),
            Tick(5_000_000)
        );
        assert_eq!(
            timeline_tick_at(-99.0, content, Tick(0), 10_000_000.0),
            Tick(0)
        );
        assert_eq!(
            timeline_tick_at(9_999.0, content, Tick(0), 10_000_000.0),
            Tick(10_000_000)
        );
    }

    fn timeline_input_frame(
        context: &egui::Context,
        state: &mut EditorState,
        events: Vec<egui::Event>,
    ) {
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                events,
                ..Default::default()
            },
            |ui| {
                timeline(ui, state, 560.0);
            },
        );
    }

    fn rendered_timeline_clip_rect(state: &EditorState, clip_id: ClipId) -> Rect {
        let geometry = state.timeline_drop_geometry.expect("timeline geometry");
        let clip = state.timeline.clip(clip_id).expect("timeline clip");
        let row_index = state
            .timeline
            .tracks
            .iter()
            .position(|track| track.id == clip.track_id)
            .expect("clip track row");
        let row_height = state
            .track_heights
            .get(&clip.track_id)
            .copied()
            .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT);
        let row_top = geometry.content.top()
            + state
                .timeline
                .tracks
                .iter()
                .take(row_index)
                .map(|track| {
                    state
                        .track_heights
                        .get(&track.id)
                        .copied()
                        .unwrap_or(DEFAULT_TIMELINE_TRACK_HEIGHT)
                })
                .sum::<f32>()
            - state.timeline_scroll_y;
        clip_rect_for(
            Rect::from_min_size(
                Pos2::new(geometry.content.left(), row_top),
                Vec2::new(geometry.content.width(), row_height),
            ),
            geometry.content,
            clip.start,
            clip.duration,
            geometry.view_start,
            geometry.visible_ticks,
        )
    }

    fn drag_timeline_pointer(
        context: &egui::Context,
        state: &mut EditorState,
        press: Pos2,
        target: Pos2,
    ) {
        timeline_input_frame(
            context,
            state,
            vec![
                egui::Event::PointerMoved(press),
                egui::Event::PointerButton {
                    pos: press,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(context, state, vec![egui::Event::PointerMoved(target)]);
        timeline_input_frame(
            context,
            state,
            vec![egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
    }

    fn editor_input_frame(
        context: &egui::Context,
        state: &mut EditorState,
        events: Vec<egui::Event>,
    ) {
        editor_input_frame_at(context, state, Vec2::new(1_200.0, 800.0), events);
    }

    fn editor_input_frame_at(
        context: &egui::Context,
        state: &mut EditorState,
        size: Vec2,
        events: Vec<egui::Event>,
    ) {
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ui| show_editor(ui, state),
        );
    }

    fn inspector_opacity_input_frame(
        context: &egui::Context,
        state: &mut EditorState,
        clip_id: ClipId,
        events: Vec<egui::Event>,
    ) -> egui::Response {
        let mut response = None;
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 100.0))),
                events,
                ..Default::default()
            },
            |ui| {
                let mut transform = state.timeline.clip(clip_id).unwrap().transform;
                let scrub = inspector_scrub_value(
                    ui,
                    "Opacity".to_owned(),
                    &mut transform.opacity,
                    nle_timeline::ClipTransform::MIN_OPACITY
                        ..=nle_timeline::ClipTransform::MAX_OPACITY,
                    InspectorScrubUnit::Percent,
                );
                mixer_live_edit(state, &scrub, |timeline| {
                    timeline.set_clip_transform(clip_id, transform)
                });
                response = Some(scrub);
            },
        );
        response.expect("opacity scrub response")
    }

    #[test]
    fn viewport_clamping_does_not_destroy_a_custom_timeline_preference() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Custom split");
        editor.timeline_height = 1_000.0;
        editor.timeline_height_is_default = false;

        editor_input_frame_at(&context, &mut editor, Vec2::new(1_200.0, 720.0), Vec::new());

        assert_eq!(editor.timeline_height, 1_000.0);
        assert!(!editor.timeline_height_is_default);
        let restored = EditorState::restore(Language::English, "Custom split", editor.snapshot())
            .expect("restore custom split");
        assert_eq!(restored.timeline_height, 1_000.0);
        assert!(!restored.timeline_height_is_default);
    }

    #[test]
    fn rendered_default_layout_favors_the_viewer_panel() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Responsive split");

        editor_input_frame_at(
            &context,
            &mut editor,
            Vec2::new(1_920.0, 1_080.0),
            Vec::new(),
        );

        let (viewer, timeline) = editor
            .rendered_panel_heights()
            .expect("rendered panel geometry");
        assert!(
            viewer > timeline,
            "timeline {timeline}px, viewer {viewer}px"
        );
        assert!(timeline > 300.0);
    }

    #[test]
    fn media_pool_pointer_drag_drops_video_on_empty_timeline() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&1].center();
        let geometry = editor.timeline_drop_geometry.unwrap();
        // Track boundaries have their own resize interaction layered above the timeline.
        // Releasing media there must still count as a drop on the timeline canvas.
        let destination = Pos2::new(
            geometry.content.center().x,
            geometry.content.top() + DEFAULT_TIMELINE_TRACK_HEIGHT,
        );

        editor_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(source),
                egui::Event::PointerButton {
                    pos: source,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(source + Vec2::new(12.0, 0.0))],
        );
        assert!(egui::DragAndDrop::has_payload_of_type::<MediaDragPayload>(
            &context
        ));
        assert_eq!(editor.active_media_drag, Some(1));
        // Cross-panel payload delivery can be consumed by an overlapping widget. The editor's
        // explicit drag ownership must still make the timeline drop succeed.
        let _ = egui::DragAndDrop::take_payload::<MediaDragPayload>(&context);
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(destination)],
        );
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: destination,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        assert_eq!(
            editor
                .timeline
                .tracks
                .iter()
                .map(|track| track.clips.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(
            editor
                .timeline
                .clip(editor.selected_timeline_clip.unwrap())
                .unwrap()
                .start,
            Tick(0)
        );
    }

    #[test]
    fn media_pool_drop_survives_scroll_area_winning_the_drag_gesture() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&1].center();
        let geometry = editor.timeline_drop_geometry.unwrap();
        let destination = geometry.content.center();

        editor_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(source),
                egui::Event::PointerButton {
                    pos: source,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert_eq!(editor.active_media_drag, Some(1));

        // Native ScrollArea interaction can prevent egui from ever creating a DnD payload.
        // The explicit media ownership must still complete the drop.
        egui::DragAndDrop::clear_payload(&context);
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(destination)],
        );
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: destination,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        assert_eq!(
            editor
                .timeline
                .tracks
                .iter()
                .map(|track| track.clips.len())
                .sum::<usize>(),
            2
        );
    }

    #[test]
    fn previous_media_row_geometry_claims_press_before_child_widget_dispatch() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&1].center();
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_200.0, 800.0))),
                events: vec![
                    egui::Event::PointerMoved(source),
                    egui::Event::PointerButton {
                        pos: source,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| claim_media_press_from_previous_layout(ui, &mut editor),
        );

        assert_eq!(editor.active_media_drag, Some(1));
    }

    #[test]
    fn native_media_drag_uses_retained_geometry_and_clears_after_release() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&1].center();
        let destination = editor.timeline_drop_geometry.unwrap().content.center();

        assert!(editor.claim_media_drag_at(source));
        assert!(editor.complete_media_drag_at(destination));
        assert_eq!(editor.timeline.clip_count(), 2);
        assert_eq!(
            editor
                .timeline
                .clip(editor.selected_timeline_clip.unwrap())
                .unwrap()
                .start,
            Tick(0)
        );
        assert!(!editor.complete_media_drag_at(destination));
        assert_eq!(editor.timeline.clip_count(), 2);
    }

    #[test]
    fn native_media_drag_over_occupied_time_performs_a_linked_overwrite() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Overwrite drop");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        editor.take_action();
        editor.timeline_view_start = Tick(0);
        editor.timeline_view_span = PROVISIONAL_MEDIA_DURATION;

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&2].center();
        let destination = editor.timeline_drop_geometry.unwrap().content.center();
        assert!(editor.claim_media_drag_at(source));
        assert!(editor.complete_media_drag_at(destination));

        let selected = editor
            .timeline
            .clip(
                editor
                    .selected_timeline_clip
                    .expect("overwritten clip selected"),
            )
            .expect("selected overwrite exists");
        assert_eq!(selected.media, TimelineMediaId(2));
        assert_eq!(selected.start, Tick(7_500_000));
        for kind in [TrackKind::Video, TrackKind::Audio] {
            let track = editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == kind)
                .expect("default edit track");
            assert_eq!(track.clips.len(), 2);
            assert_eq!(track.clips[0].media, TimelineMediaId(1));
            assert_eq!(track.clips[0].duration, Tick(7_500_000));
            assert_eq!(track.clips[1].media, TimelineMediaId(2));
        }
        assert!(matches!(
            editor.take_action(),
            Some(EditorAction::AnalyzeMedia { media_id: 2, .. })
        ));
        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.clip_count(), 2);
        assert!(editor.timeline.tracks.iter().all(|track| {
            track
                .clips
                .iter()
                .all(|clip| clip.media == TimelineMediaId(1))
        }));
    }

    #[test]
    fn native_media_drag_accepts_retained_release_when_os_sample_is_outside() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&1].center();
        let destination = editor.timeline_drop_geometry.unwrap().content.center();

        assert!(editor.claim_media_drag_at(source));
        assert!(editor.complete_media_drag_at_any([Pos2::new(-1.0, -1.0), destination,]));
        assert_eq!(editor.timeline.clip_count(), 2);
    }

    #[test]
    fn native_media_drag_can_start_from_thumbnail_edge_of_full_card() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source_rect = editor.media_drag_rects[&1];
        let thumbnail_point = Pos2::new(source_rect.left() + 8.0, source_rect.top() + 8.0);
        let destination = editor.timeline_drop_geometry.unwrap().content.center();

        assert!(source_rect.width() > 200.0);
        assert!(source_rect.height() >= 58.0);
        assert!(editor.claim_media_drag_at(thumbnail_point));
        assert!(editor.complete_media_drag_at(destination));
        assert_eq!(editor.timeline.clip_count(), 2);
    }

    #[test]
    fn packaged_layout_drag_gate_requires_rendered_source_and_drop_geometry() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        assert!(!editor.exercise_layout_backed_media_drop(1));
        editor_input_frame(&context, &mut editor, Vec::new());
        assert!(editor.exercise_layout_backed_media_drop(1));
        assert_eq!(editor.timeline.clip_count(), 2);
        assert_eq!(editor.timeline_view_span, Tick(16_000_000));
        assert_eq!(
            editor
                .timeline
                .clip(
                    editor
                        .selected_timeline_clip
                        .expect("selected inserted clip")
                )
                .expect("inserted clip")
                .start,
            Tick(0)
        );
    }

    #[test]
    fn native_media_drag_release_outside_timeline_is_cancelled() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("drag-me.mp4")]);

        editor_input_frame(&context, &mut editor, Vec::new());
        let source = editor.media_drag_rects[&1].center();

        assert!(editor.claim_media_drag_at(source));
        assert!(editor.complete_media_drag_at(Pos2::new(-1.0, -1.0)));
        assert_eq!(editor.timeline.clip_count(), 0);
        assert!(!editor.cancel_media_drag());
    }

    #[test]
    fn timeline_insert_undo_and_redo_restore_the_linked_pair() {
        let mut editor = EditorState::new(Language::English, "Undo");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let after = editor.timeline.snapshot();
        assert_eq!(editor.timeline.clip_count(), 2);

        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.clip_count(), 0);
        assert!(editor.redo_timeline());
        assert_eq!(editor.timeline.snapshot(), after);
    }

    #[test]
    fn hold_c_temporarily_uses_razor_and_release_restores_pointer() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Tools");
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::Key {
                key: egui::Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(editor.tool, TimelineTool::Razor);
        assert!(editor.held_razor);

        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::Key {
                key: egui::Key::C,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(editor.tool, TimelineTool::Pointer);
        assert!(!editor.held_razor);
    }

    #[test]
    fn command_palette_fuzzy_matches_static_command_ids_and_names() {
        let add_video = EDITOR_COMMANDS
            .iter()
            .find(|command| command.command == EditorCommand::AddVideoTrack)
            .copied()
            .unwrap();
        assert!(command_matches(add_video, "avt", Language::English));
        assert!(command_matches(add_video, "track.add", Language::Japanese));
        assert!(!command_matches(add_video, "undo", Language::English));
    }

    #[test]
    fn command_palette_commands_add_track_razor_and_delete() {
        let mut editor = EditorState::new(Language::English, "Commands");
        let initial_tracks = editor.timeline.tracks.len();
        editor.execute_command(EditorCommand::AddVideoTrack);
        assert_eq!(editor.timeline.tracks.len(), initial_tracks + 1);

        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_playhead(Tick(5_000_000));
        editor.execute_command(EditorCommand::RazorAtPlayhead);
        assert_eq!(editor.timeline.clip_count(), 4);
        editor.execute_command(EditorCommand::DeleteSelected);
        assert_eq!(editor.timeline.clip_count(), 2);
    }

    #[test]
    fn importing_two_hundred_files_queues_no_analysis_until_placement() {
        let mut editor = EditorState::new(Language::English, "Import");
        editor.add_media_paths((0..200).map(|index| PathBuf::from(format!("clip-{index}.mp4"))));
        assert_eq!(editor.media.len(), 200);
        assert!(editor.waveforms.is_empty());
        assert!(editor.video_strips.is_empty());
        assert!(editor.take_action().is_none());
    }

    #[test]
    fn pointer_centered_zoom_keeps_anchor_and_pan_never_goes_negative() {
        let mut editor = EditorState::new(Language::English, "Navigation");
        editor.timeline_view_start = Tick(10_000_000);
        editor.timeline_view_span = Tick(20_000_000);
        let anchor = Tick(20_000_000);
        editor.zoom_timeline_view(anchor, 0.5, 0.5);
        assert_eq!(editor.timeline_view_span, Tick(10_000_000));
        assert_eq!(editor.timeline_view_start, Tick(15_000_000));
        editor.pan_timeline_view(Tick(-99_000_000));
        assert_eq!(editor.timeline_view_start, Tick(0));
    }

    #[test]
    fn timeline_navigator_keeps_content_and_forward_headroom_in_range() {
        let mut editor = EditorState::new(Language::English, "Navigator");
        editor.timeline_view_span = Tick(10_000_000);
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());

        let extent = timeline_navigator_extent(&editor);
        assert!(
            extent.0 >= 20_000_000,
            "provisional 15-second media plus five seconds of headroom must be reachable"
        );
        assert!(extent.0 > editor.timeline_view_span.0);
    }

    #[test]
    fn timeline_navigator_track_click_centers_and_clamps_view() {
        let extent = Tick(100_000_000);
        let span = Tick(20_000_000);

        assert_eq!(
            timeline_navigator_start_at_fraction(0.0, extent, span),
            Tick(0)
        );
        assert_eq!(
            timeline_navigator_start_at_fraction(0.5, extent, span),
            Tick(40_000_000)
        );
        assert_eq!(
            timeline_navigator_start_at_fraction(1.0, extent, span),
            Tick(80_000_000)
        );
    }

    #[test]
    fn range_tool_drag_records_a_time_interval_without_moving_playhead() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Range");
        editor.tool = TimelineTool::Range;
        editor_input_frame(&context, &mut editor, Vec::new());
        let content = editor.timeline_drop_geometry.unwrap().content;
        // Stay within a row body. Full-width row boundaries deliberately own track resizing.
        let row_center = content.top() + DEFAULT_TIMELINE_TRACK_HEIGHT * 0.5;
        let start = Pos2::new(content.left() + content.width() * 0.2, row_center);
        let end = Pos2::new(content.left() + content.width() * 0.7, row_center);

        editor_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        editor_input_frame(&context, &mut editor, vec![egui::Event::PointerMoved(end)]);
        editor_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        let (left, right) = editor.range_selection.unwrap();
        assert!(left < right);
        assert_eq!(editor.playhead, Tick(0));
    }

    #[test]
    fn external_file_hover_over_empty_timeline_resolves_to_zero() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.set_drop_hovered(true);

        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(Pos2::new(500.0, 220.0))],
        );

        assert_eq!(
            editor.timeline_drop_start_at(Pos2::new(500.0, 220.0)),
            Some(Tick(0))
        );
        assert_eq!(editor.timeline_drop_start_at(Pos2::new(500.0, 20.0)), None);
    }

    #[derive(Default)]
    struct RecordingTimelineCanvas {
        begins: usize,
        solids: Vec<(Rect, Color32)>,
        textures: Vec<(Rect, u64, egui::TextureId, Rect, Color32)>,
    }

    impl TimelineCanvas for RecordingTimelineCanvas {
        fn begin(&mut self, _ui: &mut Ui, _canvas_rect: Rect) {
            self.begins += 1;
        }

        fn solid_rect(&mut self, rect: Rect, color: Color32) {
            self.solids.push((rect, color));
        }

        fn texture_rect(
            &mut self,
            rect: Rect,
            native_texture_id: u64,
            fallback_texture: egui::TextureId,
            uv: Rect,
            tint: Color32,
        ) {
            self.textures
                .push((rect, native_texture_id, fallback_texture, uv, tint));
        }
    }

    fn timeline_canvas_frame(
        context: &egui::Context,
        state: &mut EditorState,
        canvas: &mut dyn TimelineCanvas,
    ) {
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                ..Default::default()
            },
            |ui| timeline_with_canvas(ui, state, 560.0, canvas),
        );
    }

    #[test]
    fn solid_line_spans_both_requested_endpoints() {
        let mut canvas = RecordingTimelineCanvas::default();
        solid_line(
            &mut canvas,
            Pos2::new(100.0, 40.0),
            Pos2::new(500.0, 40.0),
            2.0,
            Color32::WHITE,
        );
        let rect = canvas.solids[0].0;
        assert_eq!(rect.left(), 100.0);
        assert_eq!(rect.right(), 500.0);
        assert_eq!(rect.center().y, 40.0);
    }

    #[test]
    fn audio_gain_line_is_a_full_width_native_timeline_primitive() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Gain rendering");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .expect("linked audio clip")
            .clone();
        editor.track_heights.insert(audio.track_id, 120.0);

        let mut canvas = RecordingTimelineCanvas::default();
        timeline_canvas_frame(&context, &mut editor, &mut canvas);
        let clip_rect = rendered_timeline_clip_rect(&editor, audio.id);
        let gain_color = Color32::from_rgb(235, 248, 238);
        let gain_rect = canvas
            .solids
            .iter()
            .find_map(|(rect, color)| (*color == gain_color).then_some(*rect))
            .expect("native gain primitive");

        assert_eq!(gain_rect.left(), clip_rect.left());
        assert_eq!(gain_rect.right(), clip_rect.right());
        assert_eq!(gain_rect.center().y, audio_gain_y(clip_rect, audio.gain_db));
    }

    #[test]
    fn timeline_flag_uses_two_clipped_fixed_rectangles() {
        let clip_rect = Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::new(80.0, 40.0));
        let flag_rects = timeline_flag_rects(clip_rect).expect("visible flag");

        assert!(
            flag_rects
                .iter()
                .all(|rect| rect.intersect(clip_rect) == *rect && rect.is_positive())
        );
        assert!(timeline_flag_rects(Rect::from_min_size(Pos2::ZERO, Vec2::splat(6.0))).is_none());
    }

    #[test]
    fn waveform_columns_are_emitted_into_the_native_instance_batch() {
        let mut canvas = RecordingTimelineCanvas::default();
        let peaks = vec![(-0.25, 0.5); 1_000];
        draw_timeline_waveform(
            &mut canvas,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 40.0)),
            &peaks,
            Color32::WHITE,
        );

        assert_eq!(canvas.solids.len(), 100);
        let first = canvas.solids[0].0;
        assert!((first.top() - 11.6).abs() < 0.001);
        assert!((first.bottom() - 24.2).abs() < 0.001);
        assert!(
            canvas
                .solids
                .iter()
                .all(|(rect, _)| rect.is_positive() && rect.width() <= 1.0)
        );
    }

    #[test]
    fn timeline_video_tiles_use_native_texture_cells_in_source_order() {
        let layout = VideoStripLayout {
            duration: Tick(4_000_000),
            frame_count: 4,
            columns: 4,
            rows: 1,
            frame_width: 160,
            frame_height: 80,
        };
        let clip = Clip {
            id: ClipId(1),
            media: TimelineMediaId(1),
            track_id: TrackId(1),
            link_id: None,
            enabled: true,
            start: Tick(0),
            duration: Tick(4_000_000),
            source_in: Tick(0),
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new(),
            transform: nle_timeline::ClipTransform::default(),
            fade_in: Default::default(),
            fade_out: Default::default(),
        };
        let strip = CachedVideoStrip {
            native_texture_id: 73,
            texture: egui::TextureId::Managed(17),
            layout,
        };
        let mut canvas = RecordingTimelineCanvas::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(480.0, 80.0));

        draw_timeline_video_tiles(&mut canvas, rect, &clip, strip);

        assert_eq!(canvas.textures.len(), 3);
        assert!(canvas.textures.iter().all(|(_, key, texture, _, tint)| {
            *key == 73 && *texture == egui::TextureId::Managed(17) && *tint == Color32::WHITE
        }));
        assert!(
            canvas
                .textures
                .windows(2)
                .all(|tiles| tiles[0].0.left() < tiles[1].0.left())
        );
        assert_eq!(canvas.textures[0].3, video_strip_frame_uv(layout, 1));
        assert_eq!(canvas.textures[1].3, video_strip_frame_uv(layout, 2));
        assert_eq!(canvas.textures[2].3, video_strip_frame_uv(layout, 3));
    }

    #[test]
    fn timeline_video_tiles_crop_the_native_texture_uv_at_clip_edges() {
        let layout = VideoStripLayout {
            duration: Tick(1_000_000),
            frame_count: 1,
            columns: 1,
            rows: 1,
            frame_width: 160,
            frame_height: 80,
        };
        let clip = Clip {
            id: ClipId(1),
            media: TimelineMediaId(1),
            track_id: TrackId(1),
            link_id: None,
            enabled: true,
            start: Tick(0),
            duration: Tick(1_000_000),
            source_in: Tick(0),
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new(),
            transform: nle_timeline::ClipTransform::default(),
            fade_in: Default::default(),
            fade_out: Default::default(),
        };
        let mut canvas = RecordingTimelineCanvas::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(80.0, 80.0));
        draw_timeline_video_tiles(
            &mut canvas,
            rect,
            &clip,
            CachedVideoStrip {
                native_texture_id: 1,
                texture: egui::TextureId::Managed(2),
                layout,
            },
        );

        assert_eq!(canvas.textures.len(), 1);
        assert_eq!(canvas.textures[0].0, rect);
        assert!((canvas.textures[0].3.right() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn native_canvas_primitives_stay_bounded_for_fifty_thousand_banded_clips() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        let track_id = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("default timeline has video")
            .id;
        let mut snapshot = editor.timeline.snapshot();
        let track = snapshot
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .unwrap();
        for index in 0..50_000_i64 {
            track.clips.push(Clip {
                id: ClipId(index as u32 + 1),
                media: TimelineMediaId(1),
                track_id,
                link_id: None,
                enabled: true,
                start: Tick(index * 2_000),
                duration: Tick(1_000),
                source_in: Tick(0),
                gain_db: 0.0,
                gain_left_db: 0.0,
                gain_right_db: 0.0,
                effects: Vec::new(),
                video_effects: Vec::new(),
                transform: nle_timeline::ClipTransform::default(),
                fade_in: Default::default(),
                fade_out: Default::default(),
            });
        }
        editor.timeline = Timeline::from_snapshot(snapshot).unwrap();
        editor.timeline_view_span = Tick(100_000_000);
        let mut canvas = RecordingTimelineCanvas::default();
        timeline_canvas_frame(&context, &mut editor, &mut canvas);

        assert_eq!(canvas.begins, 1);
        // One primitive per horizontal screen-pixel band plus the fixed timeline chrome.  The
        // threshold deliberately leaves room for six track rows, ruler lines and the playhead.
        assert!(
            canvas.solids.len() <= 1_100,
            "{} solids",
            canvas.solids.len()
        );
    }

    #[test]
    #[ignore = "manual release interaction evidence; run with --release --ignored --nocapture"]
    fn fifty_thousand_clip_editor_history_events_stay_under_two_ms() {
        use std::time::{Duration, Instant};

        let mut editor = EditorState::new(Language::English, "50k history evidence");
        let track_id = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("default timeline has video")
            .id;
        let mut snapshot = editor.timeline.snapshot();
        snapshot
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .unwrap()
            .clips = (1..=50_000_u32)
            .map(|id| Clip {
                id: ClipId(id),
                media: TimelineMediaId(1),
                track_id,
                link_id: None,
                enabled: true,
                start: Tick(i64::from(id - 1) * 2_000),
                duration: Tick(1_000),
                source_in: Tick(0),
                gain_db: 0.0,
                gain_left_db: 0.0,
                gain_right_db: 0.0,
                effects: Vec::new(),
                video_effects: Vec::new(),
                transform: nle_timeline::ClipTransform::default(),
                fade_in: Default::default(),
                fade_out: Default::default(),
            })
            .collect();
        editor.timeline = Timeline::from_snapshot(snapshot).unwrap();

        let press_started = Instant::now();
        editor.begin_timeline_history();
        let press_elapsed = press_started.elapsed();

        let release_started = Instant::now();
        editor
            .timeline
            .move_clip_with_link(ClipId(25_000), Tick(-49_997_000), false)
            .unwrap();
        assert!(editor.commit_timeline_history());
        let release_elapsed = release_started.elapsed();

        eprintln!(
            "50k editor history events: press checkpoint={press_elapsed:?}, edit+release={release_elapsed:?}"
        );
        assert!(
            press_elapsed < Duration::from_millis(2),
            "50k pointer-press checkpoint exceeded 2 ms: {press_elapsed:?}"
        );
        assert!(
            release_elapsed < Duration::from_millis(2),
            "50k edit/release history exceeded 2 ms: {release_elapsed:?}"
        );
        assert!(editor.undo_timeline());
        assert_eq!(
            editor.timeline.clip(ClipId(25_000)).unwrap().start.0,
            49_998_000
        );
        assert!(editor.redo_timeline());
        assert_eq!(editor.timeline.clip(ClipId(25_000)).unwrap().start.0, 1_000);
    }

    #[test]
    fn ruler_scrub_keeps_ownership_and_updates_playhead_every_pointer_frame() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_playhead(Tick(7_500_000));
        editor.set_detail_zoom();

        timeline_input_frame(&context, &mut editor, vec![]);
        let scrub_id =
            egui::Id::new((egui::ViewportId::ROOT, "__top_ui")).with("timeline-playhead-scrub");
        let handle = context
            .read_response(scrub_id)
            .expect("timeline should register its ruler handle")
            .rect
            .center();
        let primary = egui::PointerButton::Primary;
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(handle),
                egui::Event::PointerButton {
                    pos: handle,
                    button: primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(editor.is_scrubbing(), "ruler press should own the scrub");

        let right = handle + Vec2::new(260.0, 108.0);
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(right)],
        );
        let right_tick = editor.playhead;
        assert!(right_tick > Tick(7_500_000));
        assert!(editor.is_scrubbing(), "clip area must not steal ruler drag");

        let left = handle + Vec2::new(-260.0, 208.0);
        timeline_input_frame(&context, &mut editor, vec![egui::Event::PointerMoved(left)]);
        assert!(editor.playhead < right_tick);
        assert!(
            editor.is_scrubbing(),
            "drag ownership must persist each frame"
        );

        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: left,
                button: primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!editor.is_scrubbing());
    }

    #[test]
    fn quiet_waveforms_are_normalized_but_silence_stays_flat() {
        let quiet = normalize_waveform_display(vec![(-0.003, 0.004); 16]);
        assert!(
            quiet
                .iter()
                .all(|(low, high)| low.is_finite() && high.is_finite())
        );
        assert!(
            quiet
                .iter()
                .any(|(low, high)| low.abs().max(high.abs()) > 0.5)
        );
        assert!(quiet.iter().all(|(low, high)| *low >= -1.0 && *high <= 1.0));

        let silent = normalize_waveform_display(vec![(0.0, 0.0), (f32::NAN, f32::INFINITY)]);
        assert_eq!(silent, vec![(0.0, 0.0), (0.0, 0.0)]);
    }

    #[test]
    fn waveform_normalization_ignores_single_loud_outlier() {
        let mut peaks = vec![(-0.003, 0.004); 20];
        peaks.push((-1.0, 1.0));
        let normalized = normalize_waveform_display(peaks);
        assert!(normalized[0].1 > 0.5);
        assert_eq!(normalized.last().copied(), Some((-1.0, 1.0)));
    }

    #[test]
    fn audio_gain_uses_the_available_row_height_and_round_trips() {
        let rect = Rect::from_min_max(Pos2::new(20.0, 100.0), Pos2::new(420.0, 300.0));
        assert_eq!(audio_gain_y(rect, 0.0), rect.center().y);
        assert_eq!(
            audio_gain_at_y(rect, audio_gain_y(rect, MAX_GAIN_DB)),
            MAX_GAIN_DB
        );
        assert_eq!(
            audio_gain_at_y(rect, audio_gain_y(rect, MIN_GAIN_DB)),
            MIN_GAIN_DB
        );
        let minus_twelve = audio_gain_y(rect, -12.0);
        assert!((audio_gain_at_y(rect, minus_twelve) + 12.0).abs() < 0.001);

        let content = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(400.0, 32.0));
        let clip_rect = clip_rect_for(
            content,
            content,
            Tick(0),
            Tick(5_000_000),
            Tick(0),
            10_000_000.0,
        );
        let rendered_y = audio_gain_y(clip_rect, 24.0);
        assert!((audio_gain_at_y(clip_rect, rendered_y) - 24.0).abs() < 0.001);
        assert!((audio_gain_at_y(content, rendered_y) - 24.0).abs() > 1.0);
    }

    #[test]
    fn pressing_the_audio_gain_line_claims_the_gesture_immediately() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Gain press");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .expect("linked audio clip")
            .clone();
        editor.track_heights.insert(audio.track_id, 120.0);

        timeline_input_frame(&context, &mut editor, Vec::new());
        let clip_rect = rendered_timeline_clip_rect(&editor, audio.id);
        let press = Pos2::new(clip_rect.center().x, audio_gain_y(clip_rect, 0.0));
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(press),
                egui::Event::PointerButton {
                    pos: press,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );

        assert_eq!(editor.timeline_drag, Some(TimelineDrag::Gain(audio.id)));
    }

    #[test]
    fn dragging_the_visible_audio_gain_line_updates_the_playback_target() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Gain gesture");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .expect("linked audio clip")
            .clone();
        editor.track_heights.insert(audio.track_id, 120.0);

        timeline_input_frame(&context, &mut editor, Vec::new());
        let clip_rect = rendered_timeline_clip_rect(&editor, audio.id);
        let press = Pos2::new(clip_rect.center().x, audio_gain_y(clip_rect, 0.0));
        let target = Pos2::new(press.x, audio_gain_y(clip_rect, -12.0));
        drag_timeline_pointer(&context, &mut editor, press, target);

        let stored_gain = editor.timeline.clip(audio.id).expect("audio clip").gain_db;
        assert!(
            (stored_gain + 12.0).abs() < 0.1,
            "stored gain was {stored_gain}"
        );
        let playback_gain = editor
            .audio_playback_targets()
            .into_iter()
            .find(|target| target.clip_id == audio.id)
            .expect("audio playback target")
            .gain_db;
        assert_eq!(playback_gain, stored_gain);

        let restored = EditorState::restore(Language::English, "Gain gesture", editor.snapshot())
            .expect("restore gain edit");
        assert_eq!(
            restored
                .timeline
                .clip(audio.id)
                .expect("restored audio")
                .gain_db,
            stored_gain
        );
        assert_eq!(
            restored
                .audio_playback_targets()
                .into_iter()
                .find(|target| target.clip_id == audio.id)
                .expect("restored playback target")
                .gain_db,
            stored_gain
        );
    }

    #[test]
    fn dragging_fade_handles_updates_video_and_audio_playback_envelopes() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Fade gestures");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .and_then(|track| track.clips.first())
            .expect("linked video clip")
            .clone();
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .expect("linked audio clip")
            .clone();
        // This regression deliberately makes the clip narrow enough for the fade curve point to
        // overlap trim-edge slop. First placement now fits content for real users, so request the
        // adversarial zoom explicitly instead of depending on the former 4:12 default.
        editor.timeline_view_start = Tick(0);
        editor.timeline_view_span = legacy_zoom_span(0.08, 0.92);
        editor.track_heights.insert(video.track_id, 120.0);
        editor.track_heights.insert(audio.track_id, 120.0);
        timeline_input_frame(&context, &mut editor, Vec::new());

        let video_rect = rendered_timeline_clip_rect(&editor, video.id);
        let video_start =
            fade_control_geometry(video_rect, video.duration, video.fade_in, FadeEdge::In)
                .full_endpoint;
        let requested_video_fade = nle_timeline::Fade {
            duration: Tick(3_000_000),
            curve: 0.0,
        };
        let video_duration_target = fade_control_geometry(
            video_rect,
            video.duration,
            requested_video_fade,
            FadeEdge::In,
        )
        .full_endpoint;
        drag_timeline_pointer(&context, &mut editor, video_start, video_duration_target);
        let created_video_fade = editor.timeline.clip(video.id).unwrap().fade_in;
        assert!(
            (created_video_fade.duration.0 - requested_video_fade.duration.0).abs() <= 20_000,
            "video fade duration was {:?}",
            created_video_fade.duration
        );

        let video_rect = rendered_timeline_clip_rect(&editor, video.id);
        let video_curve_start =
            fade_control_geometry(video_rect, video.duration, created_video_fade, FadeEdge::In)
                .curve_point;
        let video_curve_target = Pos2::new(video_curve_start.x, fade_curve_y(video_rect, 1.0));
        assert!(matches!(
            clip_drag_hit(
                video_rect,
                TrackKind::Video,
                editor.timeline.clip(video.id).unwrap(),
                video_curve_start,
                editor.timeline_drop_geometry.unwrap().visible_ticks,
                editor.timeline_drop_geometry.unwrap().content.width(),
            ),
            Some(TimelineDrag::FadeCurve(_, FadeEdge::In))
        ));
        assert_eq!(
            clip_structural_edge_hit(video_rect, video_curve_start),
            Some(FadeEdge::In),
            "regression setup requires an overlapping trim hit zone"
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(video_curve_start),
                egui::Event::PointerButton {
                    pos: video_curve_start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(video_curve_target)],
        );
        assert!(
            matches!(
                editor.timeline_drag,
                Some(TimelineDrag::FadeCurve(_, FadeEdge::In))
            ),
            "curve drag was {:?}",
            editor.timeline_drag
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: video_curve_target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let shaped_video_fade = editor.timeline.clip(video.id).unwrap().fade_in;
        assert!(
            (shaped_video_fade.curve - 1.0).abs() < 0.01,
            "video fade curve was {}",
            shaped_video_fade.curve
        );
        editor.set_playhead(Tick(shaped_video_fade.duration.0 / 2));
        let expected_opacity = video_fade_opacity(fade_envelope_value(shaped_video_fade, 0.5));
        assert!((editor.playback_target().unwrap().opacity - expected_opacity).abs() < 0.001);

        let audio_rect = rendered_timeline_clip_rect(&editor, audio.id);
        let audio_start =
            fade_control_geometry(audio_rect, audio.duration, audio.fade_out, FadeEdge::Out)
                .full_endpoint;
        let requested_audio_fade = nle_timeline::Fade {
            duration: Tick(2_000_000),
            curve: 0.0,
        };
        let audio_duration_target = fade_control_geometry(
            audio_rect,
            audio.duration,
            requested_audio_fade,
            FadeEdge::Out,
        )
        .full_endpoint;
        assert!(matches!(
            clip_drag_hit(
                audio_rect,
                TrackKind::Audio,
                editor.timeline.clip(audio.id).unwrap(),
                audio_start,
                editor.timeline_drop_geometry.unwrap().visible_ticks,
                editor.timeline_drop_geometry.unwrap().content.width(),
            ),
            Some(TimelineDrag::FadeDuration(_, FadeEdge::Out))
        ));
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(audio_start),
                egui::Event::PointerButton {
                    pos: audio_start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(audio_duration_target)],
        );
        assert!(
            matches!(
                editor.timeline_drag,
                Some(TimelineDrag::FadeDuration(_, FadeEdge::Out))
            ),
            "audio duration drag was {:?}",
            editor.timeline_drag
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: audio_duration_target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let created_audio_fade = editor.timeline.clip(audio.id).unwrap().fade_out;
        assert!(
            (created_audio_fade.duration.0 - requested_audio_fade.duration.0).abs() <= 20_000,
            "audio fade duration was {:?}",
            created_audio_fade.duration
        );
        let audio_curve_start = fade_control_geometry(
            audio_rect,
            audio.duration,
            created_audio_fade,
            FadeEdge::Out,
        )
        .curve_point;
        let audio_curve_target = Pos2::new(audio_curve_start.x, fade_curve_y(audio_rect, -0.75));
        drag_timeline_pointer(&context, &mut editor, audio_curve_start, audio_curve_target);
        let shaped_audio_fade = editor.timeline.clip(audio.id).unwrap().fade_out;
        assert!((shaped_audio_fade.curve + 0.75).abs() < 0.01);
        let playback_audio = editor
            .audio_playback_targets()
            .into_iter()
            .find(|target| target.clip_id == audio.id)
            .expect("audio playback target");
        assert_eq!(playback_audio.fade_out_ticks, shaped_audio_fade.duration);
        assert_eq!(playback_audio.fade_out_curve, shaped_audio_fade.curve);
        assert!(editor.timeline_drag.is_none());
    }

    #[test]
    fn gain_readout_stays_inside_the_timeline_viewport() {
        let viewport = Rect::from_min_size(Pos2::new(100.0, 200.0), Vec2::new(500.0, 300.0));
        for pointer in [
            viewport.left_top(),
            viewport.right_top(),
            viewport.left_bottom(),
            viewport.right_bottom(),
            viewport.center(),
        ] {
            let readout = gain_readout_rect(viewport, pointer);
            assert!(viewport.contains_rect(readout));
        }
    }

    #[test]
    fn empty_drop_start_is_zero() {
        let mut editor = EditorState::new(Language::English, "Test");
        assert_eq!(media_drop_start(&editor.timeline, Tick(9_000_000)), Tick(0));
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        assert_eq!(
            media_drop_start(&editor.timeline, Tick(9_000_000)),
            Tick(9_000_000)
        );
    }

    #[test]
    fn media_drop_prefers_current_pointer_over_stale_source_interaction() {
        let source = Pos2::new(80.0, 120.0);
        let timeline = Pos2::new(900.0, 640.0);
        assert_eq!(
            drop_pointer_position(Some(timeline), None, Some(source)),
            Some(timeline)
        );
    }

    #[test]
    fn farther_handles_show_a_wider_time_range() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.set_zoom_handles(0.42, 0.58);
        editor.set_custom_timeline_view();
        let close = editor.visible_time_seconds();
        editor.set_zoom_handles(0.08, 0.92);
        editor.set_custom_timeline_view();
        assert!(editor.visible_time_seconds() > close);
    }

    #[test]
    fn trim_mode_shared_boundary_selects_roll_pair() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        assert!(editor.insert_media_at(2, Tick(15_000_000)));
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();
        assert_eq!(
            timeline_roll_pair(&editor.timeline, video.clips[0].id, FadeEdge::Out),
            Some((video.clips[0].id, video.clips[1].id))
        );
    }

    #[test]
    fn timeline_starts_with_three_video_and_audio_tracks() {
        let editor = EditorState::new(Language::English, "Test");
        assert_eq!(
            editor
                .timeline
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Video)
                .count(),
            3
        );
        assert_eq!(
            editor
                .timeline
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Audio)
                .count(),
            3
        );
    }

    #[test]
    fn every_active_unmuted_audio_track_reaches_native_playback() {
        let mut editor = EditorState::new(Language::English, "Mix");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let audio_tracks: Vec<_> = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .map(|track| track.id)
            .collect();
        editor
            .timeline
            .insert_clip(
                audio_tracks[1],
                TimelineMediaId(1),
                Tick(0),
                Tick(15_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        editor.set_playhead(Tick(1_000_000));

        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].track_id, audio_tracks[0]);
        assert_eq!(targets[1].track_id, audio_tracks[1]);
        assert_eq!(targets[0].source_tick, Tick(1_000_000));
        assert_eq!(targets[1].source_tick, Tick(3_000_000));
        let mut visited = Vec::new();
        editor.visit_audio_playback_sources(|target| {
            visited.push((target.track_id, target.source_tick));
        });
        assert_eq!(
            visited,
            vec![
                (audio_tracks[0], Tick(1_000_000)),
                (audio_tracks[1], Tick(3_000_000)),
            ]
        );

        editor
            .timeline
            .set_track_muted(audio_tracks[0], true)
            .unwrap();
        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].track_id, audio_tracks[1]);

        editor
            .timeline
            .set_track_muted(audio_tracks[0], false)
            .unwrap();
        editor
            .timeline
            .set_track_solo(audio_tracks[1], true)
            .unwrap();
        editor
            .timeline
            .set_track_audio_gain(audio_tracks[1], 6.0)
            .unwrap();
        editor.timeline.set_track_pan(audio_tracks[1], 0.5).unwrap();
        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].track_id, audio_tracks[1]);
        assert!((targets[0].gain_db - 6.0).abs() < f32::EPSILON);
        assert_eq!(targets[0].pan, 0.5);
    }

    #[test]
    fn disabled_linked_clips_leave_live_playback_and_undo_restores_them() {
        let mut editor = EditorState::new(Language::English, "Clip enable");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_playhead(Tick(1_000_000));
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .and_then(|track| track.clips.first())
            .unwrap()
            .id;
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .unwrap()
            .id;
        assert_eq!(
            editor.playback_target().map(|target| target.clip_id),
            Some(video)
        );
        assert_eq!(
            editor.audio_playback_target().map(|target| target.clip_id),
            Some(audio)
        );

        assert!(editor.set_timeline_clip_enabled(video, false));
        assert!(!editor.timeline.clip(video).unwrap().enabled);
        assert!(!editor.timeline.clip(audio).unwrap().enabled);
        assert!(editor.playback_target().is_none());
        assert!(editor.audio_playback_target().is_none());

        assert!(editor.undo_timeline());
        assert!(editor.timeline.clip(video).unwrap().enabled);
        assert!(editor.timeline.clip(audio).unwrap().enabled);
        assert_eq!(
            editor.playback_target().map(|target| target.clip_id),
            Some(video)
        );
        assert_eq!(
            editor.audio_playback_target().map(|target| target.clip_id),
            Some(audio)
        );

        editor.linked_selection = false;
        assert!(editor.set_timeline_clip_enabled(video, false));
        assert!(!editor.timeline.clip(video).unwrap().enabled);
        assert!(editor.timeline.clip(audio).unwrap().enabled);
        assert!(editor.playback_target().is_none());
        assert_eq!(
            editor.audio_playback_target().map(|target| target.clip_id),
            Some(audio)
        );
    }

    #[test]
    fn audio_playback_effects_are_enabled_and_ordered_clip_before_track() {
        let mut editor = EditorState::new(Language::English, "Audio rack");
        editor.add_media_paths([PathBuf::from("voice.wav")]);
        assert!(editor.add_selected_to_timeline());
        let audio_clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .and_then(|track| track.clips.first())
            .unwrap()
            .clone();
        editor
            .timeline
            .set_clip_audio_effects(
                audio_clip.id,
                vec![
                    AudioEffect::HighPass { hz: 80 },
                    AudioEffect::Bypassed(Box::new(AudioEffect::LowPass { hz: 18_000 })),
                    AudioEffect::Eq { hz: 3_000, db: 2.5 },
                ],
            )
            .unwrap();
        editor
            .timeline
            .set_track_audio_effects(
                audio_clip.track_id,
                vec![AudioEffect::StereoWidth { width: 1.2 }],
            )
            .unwrap();
        editor.set_playhead(Tick(100));

        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].effects,
            vec![
                AudioEffect::HighPass { hz: 80 },
                AudioEffect::Eq { hz: 3_000, db: 2.5 },
                AudioEffect::StereoWidth { width: 1.2 },
            ]
        );
    }

    #[test]
    fn audio_effect_bypass_helpers_preserve_settings_and_export_support() {
        let effect = AudioEffect::Eq {
            hz: 2_500,
            db: -3.0,
        };
        let bypassed = audio_effect_with_enabled(&effect, false);
        assert!(!audio_effect_enabled(&bypassed));
        assert!(!audio_effect_blocks_export(&bypassed));
        assert_eq!(audio_effect_base(&bypassed), &effect);
        assert_eq!(audio_effect_with_enabled(&bypassed, true), effect);
        assert!(audio_effect_blocks_export(&AudioEffect::Limiter));
    }

    #[test]
    fn audio_effect_scope_setters_record_independent_clip_and_track_history() {
        let mut editor = EditorState::new(Language::English, "Audio rack scope history");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let (track_id, clip_id) = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio && !track.clips.is_empty())
            .map(|track| (track.id, track.clips[0].id))
            .expect("placed media has an audio clip");

        apply_track_header_edit(&mut editor, |timeline| {
            AudioEffectsScope::Clip(clip_id)
                .set_effects(timeline, vec![AudioEffect::HighPass { hz: 80 }])
        });
        apply_track_header_edit(&mut editor, |timeline| {
            AudioEffectsScope::Track(track_id)
                .set_effects(timeline, vec![AudioEffect::StereoWidth { width: 1.2 }])
        });
        assert_eq!(
            AudioEffectsScope::Clip(clip_id).effects(&editor.timeline),
            Some([AudioEffect::HighPass { hz: 80 }].as_slice())
        );
        assert_eq!(
            AudioEffectsScope::Track(track_id).effects(&editor.timeline),
            Some([AudioEffect::StereoWidth { width: 1.2 }].as_slice())
        );

        assert!(editor.undo_timeline());
        assert!(
            AudioEffectsScope::Track(track_id)
                .effects(&editor.timeline)
                .is_some_and(|effects| effects.is_empty())
        );
        assert!(editor.undo_timeline());
        assert!(
            AudioEffectsScope::Clip(clip_id)
                .effects(&editor.timeline)
                .is_some_and(|effects| effects.is_empty())
        );
    }

    #[test]
    fn focusing_an_audio_track_clears_timeline_selection_and_opens_audio_tab() {
        let mut editor = EditorState::new(Language::English, "Audio track focus");
        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .map(|track| track.id)
            .expect("default layout includes audio");
        editor.selected_timeline_clip = Some(ClipId(999));
        editor.selected_title = Some(TitleId(999));
        editor.right_sidebar_tab = RightSidebarTab::Inspector;

        focus_audio_track_in_audio_tab(&mut editor, audio_track);

        assert_eq!(editor.undertow_track, Some(audio_track));
        assert_eq!(editor.selected_timeline_clip, None);
        assert_eq!(editor.selected_title, None);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Audio);
    }

    #[test]
    fn equal_power_crossfade_emits_two_same_track_sources_and_keeps_other_tracks() {
        let mut editor = EditorState::new(Language::English, "Audio crossfade preview");
        editor.add_media_paths([
            PathBuf::from("left.wav"),
            PathBuf::from("right.wav"),
            PathBuf::from("bed.wav"),
        ]);
        for media in &mut editor.media {
            media.duration = Some(Tick(6_000_000));
        }
        let tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let left = editor
            .timeline
            .insert_clip(
                tracks[0],
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                tracks[0],
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let bed = editor
            .timeline
            .insert_clip(
                tracks[1],
                TimelineMediaId(3),
                Tick(0),
                Tick(4_000_000),
                Tick(0),
            )
            .unwrap();
        let before = editor.timeline_history_checkpoint();
        editor
            .timeline
            .add_audio_transition(tracks[0], left, right, Tick(1_000_000))
            .unwrap();
        editor.record_timeline_history(before);

        editor.set_playhead(Tick(1_500_000));
        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 3);
        assert_eq!((targets[0].clip_id, targets[1].clip_id), (left, right));
        assert_eq!(targets[2].clip_id, bed);
        assert_eq!(targets[0].source_tick, Tick(2_500_000));
        assert_eq!(targets[1].source_tick, Tick(500_000));
        assert_eq!(targets[0].clip_tick, Tick(1_500_000));
        assert_eq!(targets[1].clip_tick, Tick(-500_000));
        assert_eq!(
            targets[0].transition,
            Some(AudioPlaybackTransitionEnvelope {
                role: AudioPlaybackTransitionRole::Outgoing,
                start_clip_tick: Tick(1_500_000),
                duration_ticks: Tick(1_000_000),
            })
        );
        assert_eq!(
            targets[1].transition,
            Some(AudioPlaybackTransitionEnvelope {
                role: AudioPlaybackTransitionRole::Incoming,
                start_clip_tick: Tick(-500_000),
                duration_ticks: Tick(1_000_000),
            })
        );
        drop(targets);

        editor.set_playhead(Tick(2_000_000));
        let targets = editor.audio_playback_targets();
        assert_eq!(targets[0].clip_tick, Tick(2_000_000));
        assert_eq!(targets[1].clip_tick, Tick(0));
        drop(targets);

        editor.set_playhead(Tick(2_500_000));
        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 2);
        assert_eq!((targets[0].clip_id, targets[1].clip_id), (right, bed));
        assert!(targets[0].transition.is_none());
        drop(targets);

        assert!(editor.undo_timeline());
        assert!(editor.timeline.audio_transitions().is_empty());
        assert!(editor.redo_timeline());
        assert_eq!(editor.timeline.audio_transitions().len(), 1);

        editor
            .timeline
            .set_clip_enabled(left, false, false)
            .unwrap();
        editor.set_playhead(Tick(2_000_000));
        let targets = editor.audio_playback_targets();
        assert_eq!(targets.len(), 2);
        assert_eq!((targets[0].clip_id, targets[1].clip_id), (right, bed));
        assert!(targets[0].transition.is_none());
    }

    #[test]
    fn audio_crossfade_capacity_requires_real_unused_source_handles() {
        let mut editor = EditorState::new(Language::English, "Audio crossfade handles");
        editor.add_media_paths([PathBuf::from("left.wav"), PathBuf::from("right.wav")]);
        editor.media[0].duration = Some(Tick(2_000_000));
        editor.media[1].duration = Some(Tick(2_000_000));
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, TimelineMediaId(1), Tick(0), Tick(2_000_000), Tick(0))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        assert_eq!(
            editor.audio_transition_duration_capacity(left, right, None),
            Some(Tick(0))
        );

        editor.media[0].duration = Some(Tick(4_000_000));
        editor.media[1].duration = Some(Tick(4_000_000));
        assert_eq!(
            editor.audio_transition_duration_capacity(left, right, None),
            Some(Tick(2_000_001))
        );
    }

    #[test]
    fn audio_crossfade_context_toggle_adds_removes_and_undoes() {
        let mut editor = EditorState::new(Language::English, "Audio crossfade toggle");
        editor.add_media_paths([PathBuf::from("left.wav"), PathBuf::from("right.wav")]);
        editor.media[0].duration = Some(Tick(5_000_000));
        editor.media[1].duration = Some(Tick(5_000_000));
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();

        assert!(editor.toggle_audio_crossfade(left, FadeEdge::Out));
        assert_eq!(editor.timeline.audio_transitions().len(), 1);
        assert!(editor.toggle_audio_crossfade(right, FadeEdge::In));
        assert!(editor.timeline.audio_transitions().is_empty());
        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.audio_transitions().len(), 1);
    }

    #[test]
    fn muted_tracks_stop_monitor_and_audio_output_and_survive_restore() {
        let mut editor = EditorState::new(Language::English, "Mute");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_playhead(Tick(1_000_000));
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video && !track.clips.is_empty())
            .unwrap()
            .id;
        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio && !track.clips.is_empty())
            .unwrap()
            .id;
        assert!(editor.playback_target().is_some());
        assert!(editor.audio_playback_target().is_some());
        editor.timeline.set_track_muted(video_track, true).unwrap();
        assert!(editor.playback_target().is_none());
        assert!(editor.audio_playback_target().is_some());
        editor.timeline.set_track_muted(audio_track, true).unwrap();
        assert!(editor.audio_playback_target().is_none());

        let restored = EditorState::restore(
            Language::English,
            "Mute",
            serde_json::from_str(&serde_json::to_string(&editor.snapshot()).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(restored.timeline.track(video_track).unwrap().muted);
        assert!(restored.timeline.track(audio_track).unwrap().muted);
    }

    #[test]
    fn track_header_mute_button_toggles_durable_state() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Mute button");
        timeline_input_frame(&context, &mut editor, vec![]);
        let geometry = editor.timeline_drop_geometry.unwrap();
        let point = Pos2::new(geometry.rect.left() + 52.0, geometry.content.top() + 16.0);
        let track = editor.timeline.tracks[0].id;
        let generation = editor.durable_generation();
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(editor.timeline.track(track).unwrap().muted);
        assert!(editor.durable_generation() > generation);
    }

    #[test]
    fn track_header_solo_button_toggles_audio_track() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Solo button");
        timeline_input_frame(&context, &mut editor, vec![]);
        let geometry = editor.timeline_drop_geometry.unwrap();
        let audio_index = editor
            .timeline
            .tracks
            .iter()
            .position(|track| track.kind == TrackKind::Audio)
            .expect("default layout includes audio");
        let track_h = DEFAULT_TIMELINE_TRACK_HEIGHT;
        let point = Pos2::new(
            geometry.rect.left() + 78.0,
            geometry.content.top() + track_h * audio_index as f32 + 16.0,
        );
        let track = editor.timeline.tracks[audio_index].id;
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(editor.timeline.track(track).unwrap().solo);
    }

    #[test]
    fn audio_track_header_click_focuses_track_without_moving_the_timeline_view() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Audio track header focus");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        timeline_input_frame(&context, &mut editor, vec![]);
        let geometry = editor.timeline_drop_geometry.unwrap();
        let audio_index = editor
            .timeline
            .tracks
            .iter()
            .position(|track| track.kind == TrackKind::Audio)
            .expect("default layout includes audio");
        let track = editor.timeline.tracks[audio_index].id;
        let point = Pos2::new(
            geometry.rect.left() + 20.0,
            geometry.content.top() + DEFAULT_TIMELINE_TRACK_HEIGHT * audio_index as f32 + 16.0,
        );
        let scroll_before = editor.timeline_scroll_y;
        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        assert_eq!(editor.undertow_track, Some(track));
        assert_eq!(editor.selected_timeline_clip, None);
        assert_eq!(editor.selected_title, None);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Audio);
        assert_eq!(editor.timeline_scroll_y, scroll_before);
    }

    #[test]
    fn selected_video_places_linked_pair_and_requests_waveform() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap();
        assert_eq!(video.clips[0].start, Tick(0));
        assert_eq!(video.clips[0].duration, Tick(15_000_000));
        assert_eq!(video.clips[0].link_id, audio.clips[0].link_id);
        assert!(matches!(
            editor.take_action(),
            Some(EditorAction::AnalyzeMedia { media_id: 1, .. })
        ));
    }

    #[test]
    fn later_media_appends_without_overlapping_the_first_pair() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.take_action();
        editor.selected_media = Some(2);
        assert!(editor.add_selected_to_timeline());

        for track in editor.timeline.tracks.iter().filter(|track| {
            matches!(track.kind, TrackKind::Video | TrackKind::Audio) && !track.clips.is_empty()
        }) {
            assert_eq!(track.clips.len(), 2);
            assert_eq!(track.clips[0].start, Tick(0));
            assert_eq!(track.clips[1].start, Tick(15_000_000));
        }
        editor.timeline.check_invariants().unwrap();
    }

    #[test]
    fn explicit_video_placement_uses_default_linked_tracks() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);

        assert!(editor.insert_media_at(1, Tick(7_500_000)));
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap();
        assert_eq!(video.clips[0].start, Tick(7_500_000));
        assert_eq!(audio.clips[0].start, Tick(7_500_000));
        assert_eq!(video.clips[0].link_id, audio.clips[0].link_id);
        editor.timeline.check_invariants().unwrap();
    }

    #[test]
    fn overlapping_explicit_placement_fails_without_emitting_work() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        editor.take_action();

        assert!(!editor.insert_media_at(2, Tick(1_000_000)));
        assert!(editor.take_action().is_none());
        assert_eq!(
            editor
                .timeline
                .tracks
                .iter()
                .map(|track| track.clips.len())
                .sum::<usize>(),
            2
        );
        editor.timeline.check_invariants().unwrap();
    }

    #[test]
    fn overwrite_placement_preserves_outer_tails_links_and_later_clip_positions() {
        let mut editor = EditorState::new(Language::English, "Overwrite placement");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        let original = editor
            .timeline
            .insert_linked_av_pair(TimelineMediaId(1), Tick(0), Tick(30_000_000), Tick(0))
            .unwrap();
        editor
            .timeline
            .insert_linked_av_pair(
                TimelineMediaId(1),
                Tick(40_000_000),
                Tick(10_000_000),
                Tick(30_000_000),
            )
            .unwrap();
        editor
            .set_waveform(2, Tick(10_000_000), vec![(-0.4, 0.4)])
            .unwrap();
        let before = editor.timeline.snapshot();

        assert!(editor.overwrite_media_at(2, Tick(5_000_000)));
        let mut inserted_link = None;
        for kind in [TrackKind::Video, TrackKind::Audio] {
            let clips = &editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == kind)
                .expect("default edit track")
                .clips;
            assert_eq!(clips.len(), 4);
            assert_eq!(
                clips
                    .iter()
                    .map(|clip| (clip.media, clip.start, clip.duration, clip.source_in))
                    .collect::<Vec<_>>(),
                vec![
                    (TimelineMediaId(1), Tick(0), Tick(5_000_000), Tick(0)),
                    (
                        TimelineMediaId(2),
                        Tick(5_000_000),
                        Tick(10_000_000),
                        Tick(0)
                    ),
                    (
                        TimelineMediaId(1),
                        Tick(15_000_000),
                        Tick(15_000_000),
                        Tick(15_000_000)
                    ),
                    (
                        TimelineMediaId(1),
                        Tick(40_000_000),
                        Tick(10_000_000),
                        Tick(30_000_000)
                    ),
                ]
            );
            assert_eq!(clips[0].link_id, Some(original.link_id));
            assert_eq!(clips[2].link_id, Some(original.link_id));
            if let Some(expected) = inserted_link {
                assert_eq!(clips[1].link_id, Some(expected));
            } else {
                inserted_link = clips[1].link_id;
            }
        }
        assert!(inserted_link.is_some());
        editor.timeline.check_invariants().unwrap();
        assert!(matches!(
            editor.take_action(),
            Some(EditorAction::AnalyzeMedia { media_id: 2, .. })
        ));
        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.snapshot(), before);
    }

    #[test]
    fn waveform_cache_reconciles_provisional_duration() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        editor.add_selected_to_timeline();
        editor.take_action();
        editor
            .set_waveform(1, Tick(3_000_000), vec![(-0.3, 0.5), (-0.8, 0.2)])
            .unwrap();
        assert_eq!(editor.cached_waveform(1).unwrap().peaks.len(), 2);
        assert!(editor.timeline.tracks.iter().all(|track| {
            track
                .clips
                .iter()
                .filter(|clip| clip.media == TimelineMediaId(1))
                .all(|clip| clip.duration == Tick(3_000_000))
        }));
    }

    #[test]
    fn waveform_probe_extends_untouched_placeholder_and_refits_first_view() {
        let mut editor = EditorState::new(Language::English, "Long source");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        assert_eq!(editor.timeline_view_span, Tick(16_000_000));

        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();

        assert!(editor.timeline.tracks.iter().all(|track| {
            track
                .clips
                .iter()
                .filter(|clip| clip.media == TimelineMediaId(1))
                .all(|clip| clip.duration == Tick(60_000_000))
        }));
        assert_eq!(editor.timeline_view_span, Tick(63_000_000));
        assert_eq!(editor.media[0].duration, Some(Tick(60_000_000)));
        assert!(editor.provisional_clip_ids.is_empty());
        assert!(!editor.auto_fit_provisional_view);
    }

    #[test]
    fn razor_sections_keep_user_timing_when_probe_arrives() {
        let mut editor = EditorState::new(Language::English, "Cut before probe");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_playhead(Tick(7_500_000));
        assert!(editor.razor_at_playhead());

        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();

        for track in &editor.timeline.tracks {
            let durations = track
                .clips
                .iter()
                .filter(|clip| clip.media == TimelineMediaId(1))
                .map(|clip| clip.duration)
                .collect::<Vec<_>>();
            if !durations.is_empty() {
                assert_eq!(durations, vec![Tick(7_500_000), Tick(7_500_000)]);
            }
        }
        assert!(editor.provisional_clip_ids.is_empty());
    }

    #[test]
    fn probed_duration_survives_restore_and_sizes_future_placement() {
        let mut editor = EditorState::new(Language::English, "Durable duration");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert!(editor.delete_selected_timeline_clip());

        let mut restored =
            EditorState::restore(Language::English, "Durable duration", editor.snapshot()).unwrap();
        assert_eq!(restored.media[0].duration, Some(Tick(60_000_000)));
        assert!(restored.add_selected_to_timeline());
        assert!(restored.timeline.tracks.iter().all(|track| {
            track
                .clips
                .iter()
                .filter(|clip| clip.media == TimelineMediaId(1))
                .all(|clip| clip.duration == Tick(60_000_000))
        }));
        assert!(restored.provisional_clip_ids.is_empty());
    }

    #[test]
    fn pre_ownership_snapshot_migrates_the_known_placeholder_shape() {
        let mut editor = EditorState::new(Language::English, "Legacy placeholder");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut json = serde_json::to_value(editor.snapshot()).unwrap();
        let view = json
            .get_mut("view")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        view.remove("provisional_clip_ids");
        view.remove("auto_fit_provisional_view");

        let mut restored = EditorState::restore(
            Language::English,
            "Legacy placeholder",
            serde_json::from_value(json).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.provisional_clip_ids.len(), 2);
        restored
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert_eq!(restored.timeline_end(), Tick(60_000_000));
        assert_eq!(restored.timeline_view_span, Tick(63_000_000));
    }

    #[test]
    fn automatic_view_keeps_fitting_until_every_rapid_placement_resolves() {
        let mut editor = EditorState::new(Language::English, "Two pending sources");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        assert!(editor.insert_media_at(2, Tick(20_000_000)));
        assert!(editor.auto_fit_provisional_view);

        editor
            .set_waveform(1, Tick(18_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert!(editor.auto_fit_provisional_view);
        assert_eq!(editor.timeline_end(), Tick(35_000_000));

        editor
            .set_waveform(2, Tick(30_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert!(!editor.auto_fit_provisional_view);
        assert_eq!(editor.timeline_end(), Tick(50_000_000));
        assert_eq!(editor.timeline_view_span, Tick(52_500_000));
    }

    #[test]
    fn manual_timeline_navigation_stops_probe_owned_refitting() {
        let mut editor = EditorState::new(Language::English, "Manual view");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_zoom_handles(0.0, 1.0);
        let manual_view = (editor.timeline_view_start, editor.timeline_view_span);
        assert!(!editor.auto_fit_provisional_view);

        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();

        assert_eq!(
            (editor.timeline_view_start, editor.timeline_view_span),
            manual_view
        );
        assert_eq!(editor.timeline_end(), Tick(60_000_000));
    }

    #[test]
    fn placement_undo_redo_before_probe_restores_duration_ownership() {
        let mut editor = EditorState::new(Language::English, "Undo pending placement");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        assert_eq!(editor.provisional_clip_ids.len(), 2);

        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.clip_count(), 0);
        assert!(editor.provisional_clip_ids.is_empty());
        assert!(!editor.auto_fit_provisional_view);

        assert!(editor.redo_timeline());
        assert_eq!(editor.timeline.clip_count(), 2);
        assert_eq!(editor.provisional_clip_ids.len(), 2);
        assert!(editor.auto_fit_provisional_view);
        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert_eq!(editor.timeline_end(), Tick(60_000_000));
    }

    #[test]
    fn redo_after_probe_uses_durable_known_duration_not_old_placeholder() {
        let mut editor = EditorState::new(Language::English, "Redo resolved placement");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();

        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.clip_count(), 0);
        assert!(editor.redo_timeline());
        assert_eq!(editor.timeline_end(), Tick(60_000_000));
        assert!(editor.provisional_clip_ids.is_empty());
    }

    #[test]
    fn undo_delete_or_razor_restores_unresolved_clip_ownership() {
        let mut deleted = EditorState::new(Language::English, "Undo pending delete");
        deleted.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(deleted.add_selected_to_timeline());
        assert!(deleted.delete_selected_timeline_clip());
        assert!(deleted.provisional_clip_ids.is_empty());
        assert!(deleted.undo_timeline());
        assert_eq!(deleted.provisional_clip_ids.len(), 2);
        deleted
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert_eq!(deleted.timeline_end(), Tick(60_000_000));

        let mut razored = EditorState::new(Language::English, "Undo pending razor");
        razored.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(razored.add_selected_to_timeline());
        razored.set_playhead(Tick(7_500_000));
        assert!(razored.razor_at_playhead());
        assert!(razored.provisional_clip_ids.is_empty());
        assert!(razored.undo_timeline());
        assert_eq!(razored.provisional_clip_ids.len(), 2);
        razored
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert_eq!(razored.timeline_end(), Tick(60_000_000));
    }

    #[test]
    fn undo_overwrite_restores_the_replaced_sources_provisional_ownership() {
        let mut editor = EditorState::new(Language::English, "Undo pending overwrite");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        assert!(editor.insert_media_at(1, Tick(0)));
        assert!(editor.overwrite_media_at(2, Tick(0)));
        assert!(editor.provisional_clip_ids.iter().all(|clip_id| {
            editor
                .timeline
                .clip(*clip_id)
                .is_some_and(|clip| clip.media == TimelineMediaId(2))
        }));

        assert!(editor.undo_timeline());
        assert_eq!(editor.provisional_clip_ids.len(), 2);
        assert!(editor.provisional_clip_ids.iter().all(|clip_id| {
            editor
                .timeline
                .clip(*clip_id)
                .is_some_and(|clip| clip.media == TimelineMediaId(1))
        }));
        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert_eq!(editor.timeline_end(), Tick(60_000_000));
    }

    #[test]
    fn undo_restores_probe_rights_without_reclaiming_a_manual_view() {
        let mut editor = EditorState::new(Language::English, "Undo with manual view");
        editor.add_media_paths([PathBuf::from("long.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_zoom_handles(0.0, 1.0);
        let manual_view = (editor.timeline_view_start, editor.timeline_view_span);
        assert!(editor.delete_selected_timeline_clip());
        assert!(editor.undo_timeline());
        assert_eq!(editor.provisional_clip_ids.len(), 2);
        assert!(!editor.auto_fit_provisional_view);

        editor
            .set_waveform(1, Tick(60_000_000), vec![(-0.2, 0.3)])
            .unwrap();
        assert_eq!(
            (editor.timeline_view_start, editor.timeline_view_span),
            manual_view
        );
    }

    #[test]
    fn hit_helpers_prioritize_audio_gain_and_fade_handles() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.wav")]);
        editor.add_selected_to_timeline();
        let clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .clips[0]
            .clone();
        let rect = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(200.0, 32.0));
        assert!(matches!(
            clip_drag_hit(
                rect,
                TrackKind::Audio,
                &clip,
                rect.center(),
                15_000_000.0,
                200.0
            ),
            Some(TimelineDrag::Gain(_))
        ));
        assert!(matches!(
            clip_drag_hit(
                rect,
                TrackKind::Audio,
                &clip,
                rect.left_top(),
                15_000_000.0,
                200.0
            ),
            Some(TimelineDrag::FadeDuration(_, FadeEdge::In))
        ));
    }

    #[test]
    fn hover_and_press_origin_find_zero_duration_fade_and_gain_controls() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.wav")]);
        editor.add_selected_to_timeline();
        let clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .clips[0]
            .clone();
        let rect = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(200.0, 80.0));

        // The same pure lookup is used for ordinary hover and egui's press origin.
        let fade_in = Pos2::new(rect.left(), rect.top() + 4.0);
        assert!(matches!(
            clip_hit_at_pointer(rect, TrackKind::Audio, &clip, fade_in, 15_000_000.0, 200.0),
            Some(Some(TimelineDrag::FadeDuration(_, FadeEdge::In)))
        ));
        let gain = Pos2::new(rect.center().x, audio_gain_y(rect, clip.gain_db));
        assert!(matches!(
            clip_hit_at_pointer(rect, TrackKind::Audio, &clip, gain, 15_000_000.0, 200.0),
            Some(Some(TimelineDrag::Gain(_)))
        ));
        assert!(matches!(
            clip_hit_at_pointer(
                rect,
                TrackKind::Audio,
                &clip,
                Pos2::new(rect.center().x, rect.bottom() - 6.0),
                15_000_000.0,
                200.0,
            ),
            Some(None)
        ));
    }

    #[test]
    fn fade_geometry_moves_duration_handle_and_matches_curve_drag_mapping() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(200.0, 80.0));
        let fade = nle_timeline::Fade {
            duration: Tick(25),
            curve: 0.75,
        };
        let fade_in = fade_control_geometry(rect, Tick(100), fade, FadeEdge::In);
        let fade_out = fade_control_geometry(rect, Tick(100), fade, FadeEdge::Out);
        assert_eq!(fade_in.full_endpoint, Pos2::new(150.0, rect.top() + 2.0));
        assert_eq!(fade_out.full_endpoint, Pos2::new(250.0, rect.top() + 2.0));
        assert_eq!(
            fade_in.outer_endpoint,
            Pos2::new(rect.left(), rect.bottom() - 2.0)
        );
        assert_eq!(
            fade_out.outer_endpoint,
            Pos2::new(rect.right(), rect.bottom() - 2.0)
        );
        assert_eq!(fade_in.curve_point, fade_envelope_point(fade_in, 0.5));
        assert_eq!(fade_out.curve_point, fade_envelope_point(fade_out, 0.5));
        assert!(fade_in.curve_point.y > rect.top());
        assert!(fade_in.curve_point.y < rect.bottom());
        assert!((fade_curve_at_y(rect, fade_in.curve_point.y) - fade.curve).abs() < 0.001);
        let zero =
            fade_control_geometry(rect, Tick(100), nle_timeline::Fade::default(), FadeEdge::In);
        assert_eq!(zero.full_endpoint, Pos2::new(rect.left(), rect.top() + 2.0));

        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.wav")]);
        editor.add_selected_to_timeline();
        let mut clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .clips[0]
            .clone();
        clip.fade_in = nle_timeline::Fade {
            duration: Tick(clip.duration.0 / 4),
            curve: 1.0,
        };
        let overlap = fade_control_geometry(rect, clip.duration, clip.fade_in, FadeEdge::In);
        assert!(matches!(
            clip_drag_hit(
                rect,
                TrackKind::Audio,
                &clip,
                overlap.full_endpoint,
                clip.duration.0 as f32,
                rect.width(),
            ),
            Some(TimelineDrag::FadeDuration(_, FadeEdge::In))
        ));
    }

    #[test]
    fn playback_clock_steps_clamps_and_stops_at_timeline_end() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.start_playback();
        editor.advance_playback(Duration::from_millis(500));
        assert_eq!(editor.playhead, Tick(500_000));
        editor.previous_frame();
        assert_eq!(editor.playhead, Tick(466_667));
        editor.set_playhead(Tick(14_999_000));
        editor.start_playback();
        editor.advance_playback(Duration::from_millis(100));
        assert_eq!(editor.playhead, Tick(15_000_000));
        assert!(!editor.playing);
    }

    #[test]
    fn native_audio_clock_correction_is_runtime_only() {
        let mut editor = EditorState::new(Language::English, "A/V clock");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.start_playback();
        let durable_generation = editor.durable_generation();
        editor.synchronize_playback_clock(Tick(4_250_000));
        assert_eq!(editor.playhead, Tick(4_250_000));
        assert_eq!(editor.durable_generation(), durable_generation);
    }

    #[test]
    fn durable_generation_ignores_playback_clock_and_tracks_snapshot_edits() {
        let mut editor = EditorState::new(Language::English, "Durable");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let saved = editor.durable_generation();

        editor.start_playback();
        editor.advance_playback(Duration::from_millis(100));
        assert_eq!(editor.durable_generation(), saved);
        editor.set_performance_hud(1.25, 1.75, 42, 7);
        assert_eq!(editor.durable_generation(), saved);
        let diagnostics = RuntimeDiagnostics {
            monitor_requests: 12,
            monitor_dropped_frames: 3,
            audio_underrun_frames: 48,
            ..RuntimeDiagnostics::default()
        };
        editor.set_runtime_diagnostics(diagnostics);
        assert_eq!(editor.runtime_diagnostics, diagnostics);
        assert_eq!(editor.durable_generation(), saved);

        editor.set_playhead(Tick(1_000_000));
        assert_ne!(editor.durable_generation(), saved);
        let after_playhead = editor.durable_generation();
        editor.set_zoom_handles(0.20, 0.80);
        assert_ne!(editor.durable_generation(), after_playhead);
    }

    #[test]
    fn performance_hud_refreshes_once_for_timeline_insertions_and_deletions() {
        let mut editor = EditorState::new(Language::English, "HUD");
        editor.set_performance_hud(1.25, 1.75, 42, 7);
        let hud_capacity = editor.performance_hud.capacity();
        assert!(!editor.refresh_performance_hud_if_stale());

        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        assert!(editor.refresh_performance_hud_if_stale());
        assert!(editor.performance_hud.contains("2 clips"));
        assert_eq!(editor.performance_hud.capacity(), hud_capacity);
        assert!(!editor.refresh_performance_hud_if_stale());

        assert!(editor.delete_selected_timeline_clip());
        assert!(editor.refresh_performance_hud_if_stale());
        assert!(editor.performance_hud.contains("0 clips"));
        assert_eq!(editor.performance_hud.capacity(), hud_capacity);
        assert!(!editor.refresh_performance_hud_if_stale());
    }

    #[test]
    fn performance_hud_refreshes_once_for_timeline_view_changes() {
        let mut editor = EditorState::new(Language::English, "HUD");
        editor.set_performance_hud(1.25, 1.75, 42, 7);
        let initial_summary = editor.performance_hud_summary;

        editor.set_zoom_handles(0.20, 0.80);
        assert!(editor.refresh_performance_hud_if_stale());
        assert_ne!(editor.performance_hud_summary, initial_summary);
        assert!(!editor.refresh_performance_hud_if_stale());
    }

    #[test]
    fn topmost_video_target_maps_timeline_to_source_tick() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("base.mp4"), PathBuf::from("upper.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let upper = editor.timeline.add_track(TrackKind::Video);
        editor
            .timeline
            .insert_clip(
                upper,
                TimelineMediaId(2),
                Tick(1_000_000),
                Tick(4_000_000),
                Tick(5_000_000),
            )
            .unwrap();
        editor.set_playhead(Tick(2_000_000));
        let target = editor.playback_target().unwrap();
        assert_eq!(target.media_id, 2);
        assert_eq!(target.path, Path::new("upper.mp4"));
        assert_eq!(target.source_tick, Tick(6_000_000));
        assert_eq!(target.decode_tick, target.source_tick);
    }

    #[test]
    fn source_frame_index_resolves_before_at_between_and_after_boundaries() {
        let index = SourceFrameTimeIndex::new(vec![Tick(100), Tick(140), Tick(210)]).unwrap();
        assert_eq!(index.resolve(Tick(0)), Some((Tick(100), Some(Tick(40)))));
        assert_eq!(index.resolve(Tick(140)), Some((Tick(140), Some(Tick(70)))));
        assert_eq!(index.resolve(Tick(180)), Some((Tick(140), Some(Tick(70)))));
        assert_eq!(index.resolve(Tick(999)), Some((Tick(210), None)));
        assert!(SourceFrameTimeIndex::new(vec![Tick(0), Tick(0)]).is_none());
        assert!(SourceFrameTimeIndex::new(vec![Tick(-1)]).is_none());
    }

    #[test]
    fn indexed_video_target_maps_trimmed_source_to_its_held_frame_boundary() {
        let mut editor = EditorState::new(Language::English, "Indexed source");
        editor.add_media_paths([PathBuf::from("indexed.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(
                video_track,
                TimelineMediaId(1),
                Tick(1_000),
                Tick(1_000),
                Tick(50),
            )
            .unwrap();
        editor.set_media_metadata(
            1,
            MediaMetadata {
                frame_rate_ratio: SourceFrameRate::new(30, 1),
                ..Default::default()
            },
        );
        editor.set_media_frame_time_index(
            1,
            Some(SourceFrameTimeIndex::new(vec![Tick(0), Tick(100), Tick(250)]).unwrap()),
        );
        editor.set_playhead(Tick(1_025));

        let target = editor.playback_target().unwrap();
        assert_eq!(target.source_tick, Tick(75));
        assert_eq!(target.decode_tick, Tick(0));
        assert_eq!(target.source_frame_duration_tick, Some(Tick(100)));
        assert_eq!(target.source_frame_rate, None);
    }

    #[test]
    fn indexed_vfr_end_hold_uses_source_microticks_after_trim_and_slip() {
        for frame_rate in [
            ProjectFrameRate::new(30, 1).unwrap(),
            ProjectFrameRate::new(30_000, 1_001).unwrap(),
        ] {
            let mut editor =
                EditorState::new_with_frame_rate(Language::English, "Indexed end hold", frame_rate);
            editor.add_media_paths([PathBuf::from("indexed-vfr.mp4")]);
            let video_track = editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .unwrap()
                .id;
            let clip_id = editor
                .timeline
                .insert_clip(
                    video_track,
                    TimelineMediaId(1),
                    Tick(1_000_000),
                    Tick(200_000),
                    Tick(0),
                )
                .unwrap();
            editor
                .timeline
                .trim_start(clip_id, Tick(50_000), false, false)
                .unwrap();
            editor
                .timeline
                .slip_clip(clip_id, Tick(50_000), false)
                .unwrap();
            editor.set_media_frame_time_index(
                1,
                Some(
                    SourceFrameTimeIndex::new(vec![
                        Tick(0),
                        Tick(40_000),
                        Tick(110_000),
                        Tick(150_000),
                        Tick(240_000),
                        Tick(310_000),
                    ])
                    .unwrap(),
                ),
            );

            for (playhead, source_tick, decode_tick, duration) in [
                (1_050_000, 100_000, 40_000, 70_000),
                (1_060_000, 110_000, 110_000, 40_000),
                (1_060_001, 110_001, 110_000, 40_000),
                (1_150_000, 200_000, 150_000, 90_000),
                (1_190_000, 240_000, 240_000, 70_000),
                (1_200_000, 249_999, 240_000, 70_000),
                (1_150_000, 200_000, 150_000, 90_000),
                (1_060_001, 110_001, 110_000, 40_000),
            ] {
                editor.set_playhead(Tick(playhead));
                let target = editor.playback_target().unwrap();
                assert_eq!(target.source_tick, Tick(source_tick));
                assert_eq!(target.decode_tick, Tick(decode_tick));
                assert_eq!(target.source_frame_duration_tick, Some(Tick(duration)));
                assert_eq!(target.source_frame_rate, None);
            }
        }
    }

    #[test]
    fn empty_source_frame_index_preserves_cfr_end_hold() {
        let frame_rate = ProjectFrameRate::new(30, 1).unwrap();
        let mut editor =
            EditorState::new_with_frame_rate(Language::English, "Empty index", frame_rate);
        editor.add_media_paths([PathBuf::from("empty-index.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_media_metadata(
            1,
            MediaMetadata {
                frame_rate_ratio: SourceFrameRate::new(30, 1),
                ..Default::default()
            },
        );
        editor.set_media_frame_time_index(1, Some(SourceFrameTimeIndex::new(vec![]).unwrap()));
        editor.set_playhead(editor.timeline_end());

        let target = editor.playback_target().unwrap();
        assert_eq!(
            target.source_tick,
            frame_rate.frame_before_end(editor.timeline_end())
        );
        assert_eq!(target.decode_tick, target.source_tick);
        assert_eq!(target.source_frame_rate, SourceFrameRate::new(30, 1));
        assert_eq!(target.source_frame_duration_tick, None);
    }

    #[test]
    fn indexed_timing_does_not_change_still_decode_behavior() {
        let mut editor = EditorState::new(Language::English, "Indexed still");
        editor.add_media_paths([PathBuf::from("still.png")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_media_frame_time_index(
            1,
            Some(SourceFrameTimeIndex::new(vec![Tick(100), Tick(200)]).unwrap()),
        );
        editor.set_playhead(Tick(150));

        let target = editor.playback_target().unwrap();
        assert_eq!(target.decode_tick, Tick(0));
        assert_eq!(target.source_frame_rate, None);
        assert_eq!(target.source_frame_duration_tick, None);
    }

    #[test]
    fn source_frame_indexes_are_runtime_only_and_reset_on_restore_or_media_error() {
        let mut editor = EditorState::new(Language::English, "Indexed runtime");
        editor.add_media_paths([PathBuf::from("indexed.mp4")]);
        let snapshot = editor.snapshot();
        let generation = editor.durable_generation();
        editor.set_media_frame_time_index(
            1,
            Some(SourceFrameTimeIndex::new(vec![Tick(0), Tick(100)]).unwrap()),
        );
        assert_eq!(editor.snapshot(), snapshot);
        assert_eq!(editor.durable_generation(), generation);
        editor.set_media_frame_time_index(1, None);
        assert!(!editor.source_frame_time_indexes.contains_key(&1));
        editor.set_media_frame_time_index(
            1,
            Some(SourceFrameTimeIndex::new(vec![Tick(0), Tick(100)]).unwrap()),
        );
        editor.set_media_error(1, "offline");
        assert!(!editor.source_frame_time_indexes.contains_key(&1));

        let restored =
            EditorState::restore(Language::English, "Indexed runtime", snapshot).unwrap();
        assert!(restored.source_frame_time_indexes.is_empty());
    }

    #[test]
    fn playback_target_evaluates_source_time_color_keyframes() {
        let mut editor = EditorState::new(Language::English, "Animated color preview");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        let effect_id = VideoEffectId(1);
        editor
            .timeline
            .set_clip_video_effects(
                clip_id,
                vec![VideoEffectNode {
                    id: effect_id,
                    enabled: true,
                    kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
                }],
            )
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Brightness,
                Tick(0),
                -0.2,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Brightness,
                Tick(1_000_000),
                0.4,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        editor
            .timeline
            .set_color_parameter(clip_id, effect_id, ColorParameter::Contrast, 1.75)
            .unwrap();

        editor.set_playhead(Tick(500_000));
        let stack = editor.playback_target().unwrap().video_effects;
        assert_eq!(stack.len(), 1);
        let nle_timeline::EvaluatedVideoEffect::BrightnessContrast(correction) = stack.active()[0]
        else {
            panic!("expected the basic correction operation");
        };
        assert!((correction.brightness - 0.1).abs() < 0.0001);
        assert_eq!(correction.contrast, 1.75);

        let mut bypassed = editor.timeline.clip(clip_id).unwrap().video_effects.clone();
        bypassed[0].enabled = false;
        editor
            .timeline
            .set_clip_video_effects(clip_id, bypassed)
            .unwrap();
        assert!(editor.playback_target().unwrap().video_effects.is_empty());
    }

    #[test]
    fn color_effect_and_keyframe_edits_round_trip_through_editor_undo() {
        let mut editor = EditorState::new(Language::English, "Color undo");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        let effect_id = VideoEffectId(1);

        apply_track_header_edit(&mut editor, |timeline| {
            timeline.set_clip_video_effects(
                clip_id,
                vec![VideoEffectNode {
                    id: effect_id,
                    enabled: true,
                    kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
                }],
            )
        });
        apply_track_header_edit(&mut editor, |timeline| {
            timeline.set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Contrast,
                Tick(500_000),
                2.0,
                KeyframeInterpolation::Smooth,
            )
        });
        assert!(
            editor
                .timeline
                .color_keyframe(clip_id, effect_id, ColorParameter::Contrast, Tick(500_000))
                .is_some()
        );

        assert!(editor.undo_timeline());
        assert!(
            editor
                .timeline
                .color_keyframe(clip_id, effect_id, ColorParameter::Contrast, Tick(500_000))
                .is_none()
        );
        assert!(editor.undo_timeline());
        assert!(
            editor
                .timeline
                .clip(clip_id)
                .unwrap()
                .video_effects
                .is_empty()
        );
        assert!(editor.redo_timeline());
        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().video_effects.len(),
            1
        );
        assert!(editor.redo_timeline());
        assert_eq!(
            editor
                .timeline
                .color_keyframe(clip_id, effect_id, ColorParameter::Contrast, Tick(500_000))
                .unwrap()
                .interpolation,
            KeyframeInterpolation::Smooth
        );
    }

    #[test]
    fn timeline_color_keyframes_respect_trimmed_source_mapping_and_hit_priority() {
        let mut editor = EditorState::new(Language::English, "Color lane geometry");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let track_id = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let clip_id = editor
            .timeline
            .insert_clip(
                track_id,
                TimelineMediaId(1),
                Tick(20_000_000),
                Tick(4_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        let node = VideoEffectNode {
            id: VideoEffectId(7),
            enabled: true,
            kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
        };
        editor
            .timeline
            .set_clip_video_effects(clip_id, vec![node.clone()])
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                node.id,
                ColorParameter::Brightness,
                Tick(3_000_000),
                0.25,
                KeyframeInterpolation::Hold,
            )
            .unwrap();
        let clip = editor.timeline.clip(clip_id).unwrap();
        let node = clip.video_effects.first().unwrap();
        assert_eq!(
            color_keyframe_timeline_tick(clip, Tick(3_000_000)),
            Some(Tick(21_000_000))
        );
        assert_eq!(color_keyframe_timeline_tick(clip, Tick(1_999_999)), None);
        let rect = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(400.0, 64.0));
        let center = color_keyframe_marker_center(
            rect,
            rect,
            Tick(20_000_000),
            4_000_000.0,
            clip,
            ColorParameter::Brightness,
            Tick(3_000_000),
        )
        .unwrap();
        let hit = color_keyframe_hit(
            rect,
            rect,
            Tick(20_000_000),
            4_000_000.0,
            clip,
            node,
            center,
        )
        .unwrap();
        assert_eq!(hit.clip_id, clip_id);
        assert_eq!(hit.parameter, ColorParameter::Brightness);
        assert_eq!(hit.value, 0.25);
        assert_eq!(hit.interpolation, KeyframeInterpolation::Hold);

        let panned_content = rect;
        let panned_clip_rect = rect;
        let centered = color_keyframe_marker_center(
            panned_clip_rect,
            panned_content,
            Tick(21_000_000),
            2_000_000.0,
            clip,
            ColorParameter::Brightness,
            Tick(4_000_000),
        )
        .unwrap();
        assert_eq!(centered.x, panned_content.center().x);
        assert!(
            color_keyframe_marker_center(
                panned_clip_rect,
                panned_content,
                Tick(21_000_000),
                2_000_000.0,
                clip,
                ColorParameter::Brightness,
                Tick(2_500_000),
            )
            .is_none(),
            "a source key before the panned viewport must stay hidden"
        );

        let compact_clip_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 22.0));
        let compact_center = color_keyframe_marker_center(
            compact_clip_rect,
            rect,
            Tick(20_000_000),
            4_000_000.0,
            clip,
            ColorParameter::Brightness,
            Tick(3_000_000),
        )
        .unwrap();
        assert!(
            color_keyframe_hit(
                compact_clip_rect,
                rect,
                Tick(20_000_000),
                4_000_000.0,
                clip,
                node,
                compact_center,
            )
            .is_none(),
            "an invisible compact-track lane must not claim input"
        );
    }

    #[test]
    fn retiming_timeline_color_keyframe_preserves_payload_and_is_one_undo_step() {
        let mut editor = EditorState::new(Language::English, "Color keyframe retime");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        let effect_id = VideoEffectId(1);
        editor
            .timeline
            .set_clip_video_effects(clip_id, vec![default_color_effect(effect_id)])
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Contrast,
                Tick(1_000_000),
                1.7,
                KeyframeInterpolation::Smooth,
            )
            .unwrap();
        let keyframe = TimelineColorKeyframe {
            clip_id,
            effect_id,
            parameter: ColorParameter::Contrast,
            source_tick: Tick(1_000_000),
            grab_offset: Tick(0),
            value: 1.7,
            interpolation: KeyframeInterpolation::Smooth,
        };
        let before = editor.timeline_history_checkpoint();
        assert!(retime_color_keyframe(
            &mut editor.timeline,
            keyframe,
            Tick(2_000_000)
        ));
        assert!(editor.record_timeline_history(before));
        let retimed = editor
            .timeline
            .color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Contrast,
                Tick(2_000_000),
            )
            .unwrap();
        assert_eq!(retimed.value, 1.7);
        assert_eq!(retimed.interpolation, KeyframeInterpolation::Smooth);
        assert!(editor.undo_timeline());
        assert!(
            editor
                .timeline
                .color_keyframe(
                    clip_id,
                    effect_id,
                    ColorParameter::Contrast,
                    Tick(1_000_000)
                )
                .is_some()
        );
        assert!(editor.redo_timeline());
        assert!(
            editor
                .timeline
                .color_keyframe(
                    clip_id,
                    effect_id,
                    ColorParameter::Contrast,
                    Tick(2_000_000)
                )
                .is_some()
        );
        assert!(!retime_color_keyframe(
            &mut editor.timeline,
            keyframe,
            Tick(2_000_000)
        ));
    }

    #[test]
    fn timeline_color_keyframe_click_seeks_without_retiming_or_leaking_history() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Color keyframe click");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        let effect_id = VideoEffectId(1);
        editor
            .timeline
            .set_clip_video_effects(clip_id, vec![default_color_effect(effect_id)])
            .unwrap();
        let original_source_tick = Tick(1_012_345);
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Brightness,
                original_source_tick,
                0.4,
                KeyframeInterpolation::EaseOut,
            )
            .unwrap();
        timeline_input_frame(&context, &mut editor, Vec::new());
        let geometry = editor.timeline_drop_geometry.unwrap();
        let clip_rect = rendered_timeline_clip_rect(&editor, clip_id);
        let clip = editor.timeline.clip(clip_id).unwrap();
        let marker = color_keyframe_marker_center(
            clip_rect,
            geometry.content,
            geometry.view_start,
            geometry.visible_ticks,
            clip,
            ColorParameter::Brightness,
            original_source_tick,
        )
        .unwrap();

        timeline_input_frame(
            &context,
            &mut editor,
            vec![
                egui::Event::PointerMoved(marker),
                egui::Event::PointerButton {
                    pos: marker,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        timeline_input_frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: marker,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        assert!(
            editor
                .timeline
                .color_keyframe(
                    clip_id,
                    effect_id,
                    ColorParameter::Brightness,
                    original_source_tick,
                )
                .is_some(),
            "a click must not frame-quantize or retime the key"
        );
        assert_eq!(editor.playhead, original_source_tick);
        assert!(editor.pending_history.is_none());
        assert!(editor.timeline_drag.is_none());
    }

    #[test]
    fn timeline_color_keyframe_drag_retimes_in_place_and_undoes_once() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Color keyframe gesture");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        let effect_id = VideoEffectId(1);
        editor
            .timeline
            .set_clip_video_effects(clip_id, vec![default_color_effect(effect_id)])
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Contrast,
                Tick(1_000_000),
                1.6,
                KeyframeInterpolation::Hold,
            )
            .unwrap();
        timeline_input_frame(&context, &mut editor, Vec::new());
        let geometry = editor.timeline_drop_geometry.unwrap();
        let clip_rect = rendered_timeline_clip_rect(&editor, clip_id);
        let clip = editor.timeline.clip(clip_id).unwrap();
        let press = color_keyframe_marker_center(
            clip_rect,
            geometry.content,
            geometry.view_start,
            geometry.visible_ticks,
            clip,
            ColorParameter::Contrast,
            Tick(1_000_000),
        )
        .unwrap();
        let target = color_keyframe_marker_center(
            clip_rect,
            geometry.content,
            geometry.view_start,
            geometry.visible_ticks,
            clip,
            ColorParameter::Contrast,
            Tick(3_000_000),
        )
        .unwrap();

        drag_timeline_pointer(&context, &mut editor, press, target);

        let moved = editor
            .timeline
            .color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Contrast,
                Tick(3_000_000),
            )
            .expect("dragged key");
        assert_eq!(moved.value, 1.6);
        assert_eq!(moved.interpolation, KeyframeInterpolation::Hold);
        assert!(editor.pending_history.is_none());
        assert!(editor.timeline_drag.is_none());
        assert!(editor.undo_timeline());
        assert!(
            editor
                .timeline
                .color_keyframe(
                    clip_id,
                    effect_id,
                    ColorParameter::Contrast,
                    Tick(1_000_000),
                )
                .is_some()
        );
        assert!(editor.redo_timeline());
        assert!(
            editor
                .timeline
                .color_keyframe(
                    clip_id,
                    effect_id,
                    ColorParameter::Contrast,
                    Tick(3_000_000),
                )
                .is_some()
        );
    }

    #[test]
    fn color_effect_stack_reorder_and_duplicate_are_undoable_with_stable_ids() {
        let mut editor = EditorState::new(Language::English, "Color stack undo");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        apply_track_header_edit(&mut editor, |timeline| {
            timeline.set_clip_video_effects(
                clip_id,
                vec![
                    default_color_effect(VideoEffectId(1)),
                    default_color_effect(VideoEffectId(3)),
                ],
            )
        });

        let original = editor.timeline.clip(clip_id).unwrap().video_effects.clone();
        assert_eq!(next_video_effect_id(&original), Some(VideoEffectId(2)));
        let mut reordered = original.clone();
        reordered.swap(0, 1);
        apply_track_header_edit(&mut editor, |timeline| {
            timeline.set_clip_video_effects(clip_id, reordered)
        });
        assert_eq!(
            editor
                .timeline
                .clip(clip_id)
                .unwrap()
                .video_effects
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            [VideoEffectId(3), VideoEffectId(1)]
        );

        let mut duplicated = editor.timeline.clip(clip_id).unwrap().video_effects.clone();
        let mut duplicate = duplicated[0].clone();
        duplicate.id = next_video_effect_id(&duplicated).unwrap();
        duplicated.insert(1, duplicate);
        apply_track_header_edit(&mut editor, |timeline| {
            timeline.set_clip_video_effects(clip_id, duplicated)
        });
        assert_eq!(
            editor
                .timeline
                .clip(clip_id)
                .unwrap()
                .video_effects
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            [VideoEffectId(3), VideoEffectId(2), VideoEffectId(1)]
        );

        assert!(editor.undo_timeline());
        assert_eq!(
            editor
                .timeline
                .clip(clip_id)
                .unwrap()
                .video_effects
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            [VideoEffectId(3), VideoEffectId(1)]
        );
        assert!(editor.undo_timeline());
        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().video_effects,
            original
        );
        assert!(editor.redo_timeline());
        assert!(editor.redo_timeline());
        assert_eq!(
            editor
                .timeline
                .clip(clip_id)
                .unwrap()
                .video_effects
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            [VideoEffectId(3), VideoEffectId(2), VideoEffectId(1)]
        );
    }

    #[test]
    fn color_keyframe_interpolation_menu_is_complete_and_bilingual() {
        assert_eq!(
            COLOR_KEYFRAME_INTERPOLATIONS,
            [
                KeyframeInterpolation::Linear,
                KeyframeInterpolation::Smooth,
                KeyframeInterpolation::EaseIn,
                KeyframeInterpolation::EaseOut,
                KeyframeInterpolation::Hold,
            ]
        );
        assert_eq!(
            keyframe_interpolation_label(Language::English, KeyframeInterpolation::Smooth),
            "Smooth"
        );
        assert_eq!(
            keyframe_interpolation_label(Language::Japanese, KeyframeInterpolation::EaseIn),
            "イーズイン"
        );
        assert!(
            keyframe_interpolation_tooltip(Language::English, KeyframeInterpolation::EaseOut)
                .contains("decelerate")
        );
        assert!(
            keyframe_interpolation_tooltip(Language::Japanese, KeyframeInterpolation::Hold)
                .contains("維持")
        );
    }

    #[test]
    fn basic_correction_parameters_have_compact_distinct_keyframe_lanes() {
        let parameters = [
            ColorParameter::Temperature,
            ColorParameter::Tint,
            ColorParameter::Saturation,
            ColorParameter::Exposure,
            ColorParameter::Contrast,
            ColorParameter::Highlights,
            ColorParameter::Shadows,
            ColorParameter::Whites,
            ColorParameter::Blacks,
            ColorParameter::Brightness,
        ];
        let mut lanes = parameters.map(color_keyframe_lane);
        lanes.sort_unstable();
        assert_eq!(lanes, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            color_parameter_label(Language::English, ColorParameter::Temperature),
            "Temperature"
        );
        assert_eq!(
            color_parameter_label(Language::Japanese, ColorParameter::Highlights),
            "ハイライト"
        );
        assert_eq!(
            color_parameter_label(Language::English, ColorParameter::Whites),
            "Whites"
        );
    }

    #[test]
    fn vignette_parameters_have_compact_labels_and_scalar_bindings() {
        let effect = VignetteEffect::default();
        let kind = VideoEffectKind::Vignette(effect);
        let scalars = video_effect_scalars(&kind);
        let parameters = scalars
            .into_iter()
            .flatten()
            .map(|(parameter, _)| parameter)
            .collect::<Vec<_>>();

        assert_eq!(
            parameters,
            vec![
                ColorParameter::VignetteAmount,
                ColorParameter::VignetteMidpoint,
                ColorParameter::VignetteFeather,
                ColorParameter::VignetteCenterX,
                ColorParameter::VignetteCenterY,
            ]
        );
        assert_eq!(
            color_parameter_label(Language::English, ColorParameter::VignetteFeather),
            "Vignette Feather"
        );
        assert_eq!(
            color_parameter_label(Language::Japanese, ColorParameter::VignetteCenterX),
            "ビネット中心 X"
        );
        assert_eq!(color_keyframe_lane(ColorParameter::VignetteCenterY), 4);
    }

    #[test]
    fn curve_editor_helpers_preserve_identity_and_8_bit_point_separation() {
        let curve = ColorCurve::default();
        assert_eq!(curve_sample(&curve, 0.25), 0.25);
        assert!(can_insert_curve_point(&curve, 0.5));
        assert!(!can_insert_curve_point(&curve, 1.0 / 510.0));
        let bent = ColorCurve {
            points: vec![
                CurvePoint { x: 0.0, y: 0.0 },
                CurvePoint { x: 0.5, y: 0.8 },
                CurvePoint { x: 1.0, y: 1.0 },
            ],
        };
        assert!((curve_sample(&bent, 0.5) - 0.8).abs() < 0.0001);
        assert!((curve_sample(&bent, 0.75) - 0.95625).abs() < 0.0001);

        let imported_tight = ColorCurve {
            points: vec![
                CurvePoint { x: 0.0, y: 0.0 },
                CurvePoint { x: 0.0078, y: 0.2 },
                CurvePoint { x: 0.0079, y: 0.8 },
                CurvePoint { x: 0.0118, y: 0.4 },
                CurvePoint { x: 1.0, y: 1.0 },
            ],
        };
        assert_eq!(
            constrained_curve_point_x(&imported_tight, 2, 0.5),
            imported_tight.points[2].x,
            "a valid imported tight curve must remain draggable without an inverted clamp panic"
        );
    }

    #[test]
    fn inspector_color_time_clamps_to_selected_clip_source_window() {
        let mut editor = EditorState::new(Language::English, "Inspector color time");
        editor.add_media_paths([PathBuf::from("color.mp4")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let clip_id = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(1_000_000),
                Tick(4_000_000),
                Tick(5_000_000),
            )
            .unwrap();
        let clip = editor.timeline.clip(clip_id).unwrap().clone();
        editor.set_playhead(Tick(0));
        assert_eq!(inspector_source_tick(&editor, &clip), Tick(5_000_000));
        editor.set_playhead(Tick(3_000_000));
        assert_eq!(inspector_source_tick(&editor, &clip), Tick(7_000_000));
        editor.set_playhead(editor.timeline_end());
        assert_eq!(inspector_source_tick(&editor, &clip), Tick(9_000_000));
    }

    #[test]
    fn playback_targets_return_the_top_four_visible_layers_bottom_to_top() {
        assert_eq!(PREVIEW_VIDEO_LAYER_COUNT, 4);
        let mut editor = EditorState::new(Language::English, "Four layers");
        editor.add_media_paths([
            PathBuf::from("base.mp4"),
            PathBuf::from("middle.mp4"),
            PathBuf::from("top.mp4"),
            PathBuf::from("foreground.mp4"),
        ]);
        let mut video_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        video_tracks.push(editor.timeline.add_track(TrackKind::Video));
        for (track, media_id, source_in) in [
            (video_tracks[0], 1, Tick(10)),
            (video_tracks[1], 2, Tick(20)),
            (video_tracks[2], 3, Tick(30)),
            (video_tracks[3], 4, Tick(40)),
        ] {
            editor
                .timeline
                .insert_clip(
                    track,
                    TimelineMediaId(media_id),
                    Tick(0),
                    Tick(100),
                    source_in,
                )
                .unwrap();
        }
        editor.set_playhead(Tick(25));

        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets.len(), PREVIEW_VIDEO_LAYER_COUNT);
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.media_id, target.source_tick))
                .collect::<Vec<_>>(),
            [(1, Tick(35)), (2, Tick(45)), (3, Tick(55)), (4, Tick(65))]
        );
        assert_eq!(editor.playback_target().unwrap().media_id, 4);

        editor
            .timeline
            .set_track_muted(video_tracks[3], true)
            .unwrap();
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(
            targets
                .iter()
                .map(|target| target.media_id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn cross_dissolve_resolves_two_continuous_sources_without_dipping_the_base() {
        let mut editor = EditorState::new(Language::English, "Transition preview");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(10_000_000));
        editor.media[1].duration = Some(Tick(10_000_000));
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_000), 0.0)
            .unwrap();

        editor.set_playhead(Tick(2_000_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].clip_id, left);
        assert_eq!(targets[0].source_tick, Tick(3_000_000));
        assert_eq!(
            targets[0].opacity, 1.0,
            "the outgoing image remains the base"
        );
        assert_eq!(targets[1].clip_id, right);
        assert_eq!(targets[1].source_tick, Tick(1_000_000));
        assert!((targets[1].opacity - 0.5).abs() < 0.0001);

        editor.set_playhead(Tick(1_750_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets[0].source_tick, Tick(2_750_000));
        assert_eq!(targets[1].source_tick, Tick(750_000));
        assert!(targets[1].opacity > 0.0 && targets[1].opacity < 0.5);

        editor
            .timeline
            .set_clip_enabled(left, false, false)
            .unwrap();
        editor.set_playhead(Tick(2_000_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].clip_id, right);
        assert_eq!(targets[0].opacity, 1.0);
    }

    #[test]
    fn dip_to_black_uses_one_trimmed_source_per_side_and_reaches_black_at_the_cut() {
        let mut editor = EditorState::new(Language::English, "Dip to black preview");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(2_000_000));
        editor.media[1].duration = Some(Tick(2_000_000));
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, TimelineMediaId(1), Tick(0), Tick(2_000_000), Tick(0))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(0),
            )
            .unwrap();
        editor.selected_timeline_clip = Some(left);
        assert!(editor.add_video_transition(FadeEdge::Out, VideoTransitionKind::DipToBlack));
        assert_eq!(
            editor.timeline.transitions()[0].kind,
            VideoTransitionKind::DipToBlack
        );

        editor.set_playhead(Tick(1_500_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].clip_id, left);
        assert_eq!(targets[0].opacity, 1.0);

        editor.set_playhead(Tick(1_750_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets[0].clip_id, left);
        assert_eq!(targets[0].opacity, 1.0);
        assert!((targets[0].black_matte_after - 0.5).abs() < 0.0001);

        editor.set_playhead(Tick(2_000_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].clip_id, right);
        assert_eq!(targets[0].source_tick, Tick(0));
        assert_eq!(targets[0].opacity, 0.0);
        assert_eq!(targets[0].black_matte_before, 1.0);

        editor.set_playhead(Tick(2_250_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets[0].clip_id, right);
        assert!((targets[0].opacity - 0.5).abs() < 0.0001);
        assert_eq!(targets[0].black_matte_before, 1.0);

        assert!(editor.undo_timeline());
        assert!(editor.timeline.transitions().is_empty());
        assert!(editor.redo_timeline());
        assert_eq!(
            editor.timeline.transitions()[0].kind,
            VideoTransitionKind::DipToBlack
        );

        assert!(editor.undo_timeline());
        assert!(
            !editor.add_cross_dissolve(FadeEdge::Out),
            "cross dissolves still require unused frames beyond both trims"
        );
    }

    #[test]
    fn upper_track_dip_inserts_opaque_black_between_lower_and_incoming_layers() {
        let mut editor = EditorState::new(Language::English, "Layered dip preview");
        editor.add_media_paths([
            PathBuf::from("lower.mp4"),
            PathBuf::from("upper-left.mp4"),
            PathBuf::from("upper-right.mp4"),
        ]);
        let tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let lower = editor
            .timeline
            .insert_clip(
                tracks[0],
                TimelineMediaId(1),
                Tick(0),
                Tick(4_000_000),
                Tick(0),
            )
            .unwrap();
        let upper_left = editor
            .timeline
            .insert_clip(
                tracks[1],
                TimelineMediaId(2),
                Tick(0),
                Tick(2_000_000),
                Tick(0),
            )
            .unwrap();
        let upper_right = editor
            .timeline
            .insert_clip(
                tracks[1],
                TimelineMediaId(3),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(0),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition_of_kind(
                tracks[1],
                upper_left,
                upper_right,
                Tick(1_000_000),
                0.0,
                VideoTransitionKind::DipToBlack,
            )
            .unwrap();
        editor.set_playhead(Tick(2_000_000));
        let targets = editor.playback_targets().collect::<Vec<_>>();
        assert_eq!(targets.len(), 2, "the matte must not consume a decode slot");
        assert_eq!(
            (targets[0].clip_id, targets[1].clip_id),
            (lower, upper_right)
        );
        assert_eq!(targets[1].black_matte_before, 1.0);
        assert_eq!(targets[1].opacity, 0.0);
        let frame_targets = targets
            .iter()
            .map(|target| (target.media_id, target.source_tick))
            .collect::<Vec<_>>();
        drop(targets);
        for (layer, (media_id, source_tick)) in frame_targets.into_iter().enumerate() {
            editor.set_monitor_frame_for_layer(
                layer,
                egui::TextureId::Managed(200 + layer as u64),
                640,
                360,
                Some(media_id),
                Some(source_tick),
            );
        }

        let context = egui::Context::default();
        let mut canvas = RecordingViewerCanvas::default();
        let _ = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                ..Default::default()
            },
            |ui| viewer_with_canvas(ui, &mut editor, &mut canvas),
        );
        assert_eq!(
            canvas.events,
            vec![
                ViewerEvent::Layer {
                    slot: 0,
                    clip: lower,
                    opacity: 1.0,
                },
                ViewerEvent::BlackMatte(1.0),
                ViewerEvent::Layer {
                    slot: 1,
                    clip: upper_right,
                    opacity: 0.0,
                },
            ]
        );
    }

    #[test]
    fn transition_creation_requires_real_source_handles_and_is_undoable() {
        let mut editor = EditorState::new(Language::English, "Transition handles");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(6_000_000));
        editor.media[1].duration = Some(Tick(6_000_000));
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor.selected_timeline_clip = Some(left);
        assert!(editor.add_cross_dissolve(FadeEdge::Out));
        assert_eq!(editor.timeline.transitions().len(), 1);
        assert_eq!(editor.timeline.transitions()[0].right_clip, right);
        assert_eq!(editor.timeline.transitions()[0].duration, Tick(1_000_000));
        assert!(editor.undo_timeline());
        assert!(editor.timeline.transitions().is_empty());
        assert!(editor.redo_timeline());
        assert_eq!(editor.timeline.transitions().len(), 1);

        assert!(editor.undo_timeline());
        editor.media[0].duration = Some(Tick(3_000_000));
        editor.media[1].duration = Some(Tick(6_000_000));
        editor
            .timeline
            .slip_clip(right, Tick(-1_000_000), false)
            .unwrap();
        assert_eq!(editor.timeline.clip(right).unwrap().source_in, Tick(0));
        assert!(!editor.add_cross_dissolve(FadeEdge::Out));
        assert!(editor.timeline.transitions().is_empty());
    }

    #[test]
    fn transition_catalog_drop_resolves_only_valid_video_cuts_and_is_undoable() {
        let mut editor = EditorState::new(Language::English, "Transition catalog drop");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(6_000_000));
        editor.media[1].duration = Some(Tick(6_000_000));
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let audio = editor.timeline.add_track(TrackKind::Audio);
        let left = editor
            .timeline
            .insert_clip(
                video,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                video,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let content = Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 200.0));
        editor.timeline_drop_geometry = Some(TimelineDropGeometry {
            rect: content,
            content,
            view_start: Tick(0),
            visible_ticks: 10_000_000.0,
        });
        editor.timeline_track_rows = vec![
            TimelineTrackRowGeometry {
                track_id: video,
                kind: TrackKind::Video,
                rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 80.0)),
            },
            TimelineTrackRowGeometry {
                track_id: audio,
                kind: TrackKind::Audio,
                rect: Rect::from_min_size(Pos2::new(0.0, 100.0), Vec2::new(1_000.0, 80.0)),
            },
        ];
        let cut = Pos2::new(200.0, 40.0);
        assert_eq!(
            editor.transition_drop_target_at(cut, VideoTransitionKind::CrossDissolve),
            Some((video, left, right))
        );
        assert!(editor.add_video_transition_at_cut(
            video,
            left,
            right,
            VideoTransitionKind::CrossDissolve
        ));
        assert_eq!(editor.timeline.transitions().len(), 1);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Effects);
        assert!(editor.undo_timeline());
        assert!(editor.timeline.transitions().is_empty());

        // Gaps, audio rows, and occupied cuts are rejected before any model mutation.
        assert!(
            editor
                .transition_drop_target_at(Pos2::new(700.0, 40.0), VideoTransitionKind::DipToBlack)
                .is_none()
        );
        assert!(
            editor
                .transition_drop_target_at(Pos2::new(200.0, 140.0), VideoTransitionKind::DipToBlack)
                .is_none()
        );
        assert!(editor.add_video_transition_at_cut(
            video,
            left,
            right,
            VideoTransitionKind::CrossDissolve
        ));
        assert!(
            editor
                .transition_drop_target_at(cut, VideoTransitionKind::DipToBlack)
                .is_none()
        );

        // Cross dissolve specifically requires unused source handles.
        editor
            .timeline
            .remove_video_transition(editor.timeline.transitions()[0].id)
            .unwrap();
        editor
            .timeline
            .slip_clip(right, Tick(-1_000_000), false)
            .unwrap();
        assert!(
            editor
                .transition_drop_target_at(cut, VideoTransitionKind::CrossDissolve)
                .is_none()
        );

        // A focus or workspace change must disarm the runtime fallback before any later release.
        editor.active_transition_drag = Some(VideoTransitionKind::DipToBlack);
        assert!(editor.cancel_transition_drag());
        assert!(editor.active_transition_drag.is_none());
        assert!(!editor.cancel_transition_drag());
    }

    #[test]
    fn transition_geometry_reveals_and_slides_the_incoming_quad() {
        let original = CompositeQuad {
            clip_id: ClipId(1),
            positions: [
                nle_compositor::Point { x: 0.0, y: 0.0 },
                nle_compositor::Point { x: 100.0, y: 0.0 },
                nle_compositor::Point { x: 100.0, y: 50.0 },
                nle_compositor::Point { x: 0.0, y: 50.0 },
            ],
            uvs: [
                nle_compositor::Uv { u: 0.0, v: 0.0 },
                nle_compositor::Uv { u: 1.0, v: 0.0 },
                nle_compositor::Uv { u: 1.0, v: 1.0 },
                nle_compositor::Uv { u: 0.0, v: 1.0 },
            ],
            opacity: 1.0,
        };
        let mut wipe = original;
        apply_transition_geometry(
            &mut wipe,
            Some(TransitionReveal::FromLeft),
            (0.25, 0.0),
            PixelSize::new(100, 50),
        );
        assert_eq!(wipe.positions[1].x, 25.0);
        assert_eq!(wipe.positions[2].x, 25.0);
        assert_eq!(wipe.uvs[1].u, 0.25);
        assert_eq!(wipe.uvs[2].u, 0.25);

        let mut slide = original;
        apply_transition_geometry(&mut slide, None, (-0.5, 0.25), PixelSize::new(100, 50));
        assert_eq!(slide.positions[0].x, -50.0);
        assert_eq!(slide.positions[0].y, 12.5);
    }

    #[test]
    fn transition_preview_progress_honors_curve_and_film_gamma() {
        let duration = Tick(1_000_000);
        let neutral_quarter = shaped_transition_progress(duration, 0.0, 0.25);
        let curved_quarter = shaped_transition_progress(duration, 0.5, 0.25);
        assert!((neutral_quarter - 0.25).abs() < f32::EPSILON);
        assert!((curved_quarter - 0.34375).abs() < f32::EPSILON);

        let film_quarter = neutral_quarter.powf(0.65);
        assert!(film_quarter > neutral_quarter);
        assert!((film_quarter - 0.4061262).abs() < 0.00001);
    }

    #[test]
    fn transition_controls_cap_duration_to_the_remaining_shared_clip_window() {
        let mut editor = EditorState::new(Language::English, "Transition overlap controls");
        editor.add_media_paths([
            PathBuf::from("left.mp4"),
            PathBuf::from("middle.mp4"),
            PathBuf::from("right.mp4"),
        ]);
        for media in &mut editor.media {
            media.duration = Some(Tick(10_000_000));
        }
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(4_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        let middle = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(4_000_000),
                Tick(2_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(3),
                Tick(6_000_000),
                Tick(4_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, middle, Tick(3_000_000), 0.0)
            .unwrap();

        assert_eq!(
            editor.transition_duration_capacity(
                middle,
                right,
                VideoTransitionKind::CrossDissolve,
                None,
            ),
            Some(Tick(1_000_001))
        );
        editor.selected_timeline_clip = Some(middle);
        assert!(editor.add_cross_dissolve(FadeEdge::Out));
        assert_eq!(
            editor.transition_at_cut(middle, right).unwrap().duration,
            Tick(1_000_000)
        );
    }

    #[test]
    fn transition_capacity_preserves_a_valid_odd_duration_tick() {
        let mut editor = EditorState::new(Language::English, "Odd transition capacity");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(3_500_001));
        editor.media[1].duration = Some(Tick(10_000_000));
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(500_000),
            )
            .unwrap();

        assert_eq!(
            editor.transition_duration_capacity(
                left,
                right,
                VideoTransitionKind::CrossDissolve,
                None,
            ),
            Some(Tick(1_000_001))
        );
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_001), 0.0)
            .expect("the exact odd-duration capacity must remain valid in the timeline");
    }

    #[test]
    fn bounded_preview_never_admits_only_half_of_a_lower_track_transition() {
        let mut editor = EditorState::new(Language::English, "Transition layer budget");
        editor.add_media_paths((1..=5).map(|id| PathBuf::from(format!("layer-{id}.mp4"))));
        let mut tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        tracks.push(editor.timeline.add_track(TrackKind::Video));
        let left = editor
            .timeline
            .insert_clip(
                tracks[0],
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                tracks[0],
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(tracks[0], left, right, Tick(1_000_000), 0.0)
            .unwrap();
        for (track, media_id) in tracks[1..].iter().copied().zip(3..=5) {
            editor
                .timeline
                .insert_clip(
                    track,
                    TimelineMediaId(media_id),
                    Tick(0),
                    Tick(4_000_000),
                    Tick(0),
                )
                .unwrap();
        }
        editor.set_playhead(Tick(2_000_000));
        assert_eq!(
            editor
                .playback_targets()
                .map(|target| target.media_id)
                .collect::<Vec<_>>(),
            [3, 4, 5]
        );
    }

    #[test]
    fn monitor_frames_are_retained_independently_per_preview_layer() {
        let mut editor = EditorState::new(Language::English, "Layer frames");
        let textures = [
            egui::TextureId::Managed(41),
            egui::TextureId::Managed(42),
            egui::TextureId::Managed(43),
            egui::TextureId::Managed(44),
        ];
        for (layer, texture) in textures.into_iter().enumerate() {
            editor.set_monitor_frame_for_layer(
                layer,
                texture,
                640,
                360,
                Some(layer as u32 + 1),
                Some(Tick((layer as i64 + 1) * 10)),
            );
        }

        for (layer, texture) in textures.into_iter().enumerate() {
            assert_eq!(
                editor.monitor_frame_for_layer(layer).unwrap().texture,
                texture
            );
        }
        editor.reset_monitor_layer(2);
        assert_eq!(
            editor.monitor_frame_for_layer(0).unwrap().texture,
            textures[0]
        );
        assert_eq!(
            editor.monitor_frame_for_layer(1).unwrap().texture,
            textures[1]
        );
        assert!(editor.monitor_frame_for_layer(2).is_none());
        assert_eq!(
            editor.monitor_frame_for_layer(3).unwrap().texture,
            textures[3]
        );
        assert_eq!(editor.monitor.unwrap().texture, textures[3]);
        assert_eq!(editor.monitor_status, MonitorStatus::Ready);
    }

    #[test]
    fn viewer_composites_two_preview_layers_in_visual_order_with_their_transforms() {
        let mut editor = EditorState::new(Language::English, "Viewer layers");
        editor.add_media_paths([PathBuf::from("base.mp4"), PathBuf::from("top.mp4")]);
        let video_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let base_clip = editor
            .timeline
            .insert_clip(
                video_tracks[0],
                TimelineMediaId(1),
                Tick(0),
                Tick(100),
                Tick(10),
            )
            .unwrap();
        let top_clip = editor
            .timeline
            .insert_clip(
                video_tracks[1],
                TimelineMediaId(2),
                Tick(0),
                Tick(100),
                Tick(20),
            )
            .unwrap();
        let base_transform = nle_timeline::ClipTransform {
            opacity: 0.25,
            scale_x: 0.5,
            scale_y: 0.5,
            pos_x: -0.25,
            ..Default::default()
        };
        let top_transform = nle_timeline::ClipTransform {
            opacity: 0.75,
            scale_x: 0.75,
            scale_y: 0.75,
            pos_x: 0.25,
            ..Default::default()
        };
        editor
            .timeline
            .set_clip_transform(base_clip, base_transform)
            .unwrap();
        editor
            .timeline
            .set_clip_transform(top_clip, top_transform)
            .unwrap();
        editor.set_playhead(Tick(25));
        let targets = editor
            .playback_targets()
            .map(|target| (target.media_id, target.source_tick))
            .collect::<Vec<_>>();
        let base_texture = egui::TextureId::Managed(44);
        let top_texture = egui::TextureId::Managed(45);
        editor.set_monitor_frame_for_layer(
            0,
            base_texture,
            640,
            360,
            Some(targets[0].0),
            Some(targets[0].1),
        );
        editor.set_monitor_frame_for_layer(
            1,
            top_texture,
            640,
            360,
            Some(targets[1].0),
            Some(targets[1].1),
        );

        let context = egui::Context::default();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                ..Default::default()
            },
            |ui| viewer(ui, &mut editor),
        );
        let images = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Mesh(mesh)
                    if mesh.texture_id == base_texture || mesh.texture_id == top_texture =>
                {
                    let min_x = mesh
                        .vertices
                        .iter()
                        .map(|vertex| vertex.pos.x)
                        .fold(f32::INFINITY, f32::min);
                    let max_x = mesh
                        .vertices
                        .iter()
                        .map(|vertex| vertex.pos.x)
                        .fold(f32::NEG_INFINITY, f32::max);
                    Some((
                        mesh.texture_id,
                        mesh.vertices[0].color.a(),
                        max_x - min_x,
                        clipped.clip_rect.max.y,
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].0, base_texture);
        assert_eq!(images[0].1, 64);
        assert_eq!(images[1].0, top_texture);
        assert_eq!(images[1].1, 191);
        assert!(
            images[0].2 < images[1].2,
            "layer scale should affect mesh width"
        );
        assert!(
            images.iter().all(|image| image.3 < 640.0),
            "layer meshes must be clipped above the transport controls"
        );
    }

    #[test]
    fn project_canvas_drives_portrait_viewer_decode_aspect_without_persisting_workspace_state() {
        let mut editor = EditorState::new(Language::English, "Portrait canvas");
        assert_eq!(editor.project_canvas_size(), (1920, 1080));
        assert!(!editor.set_project_canvas_size(0, 1));
        assert!(editor.set_project_canvas_size(1080, 1920));
        editor.add_media_paths([PathBuf::from("portrait.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(video_track, TimelineMediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        editor.set_media_metadata(
            1,
            MediaMetadata {
                width: Some(1080),
                height: Some(1920),
                ..Default::default()
            },
        );
        let texture = egui::TextureId::Managed(94);
        editor.set_monitor_frame_for_layer(0, texture, 360, 640, Some(1), Some(Tick(0)));
        let context = egui::Context::default();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                ..Default::default()
            },
            |ui| viewer(ui, &mut editor),
        );
        let (width, height) = editor.monitor_decode_size_hint();
        assert!(
            height.saturating_mul(5) > width.saturating_mul(8),
            "portrait decode hint was {width}x{height}"
        );
        assert!(height <= 720 && width <= 1280);
        let clip_rect = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Mesh(mesh) if mesh.texture_id == texture => Some(shape.clip_rect),
                _ => None,
            })
            .expect("portrait compositor mesh");
        assert!(clip_rect.width() < 400.0 && clip_rect.height() > 500.0);
        assert!(!format!("{:?}", editor.snapshot().view).contains("project_canvas_size"));
    }

    #[test]
    fn playback_target_uses_probed_source_size_and_safely_leaves_frame_fallback_available() {
        let mut editor = EditorState::new(Language::English, "Source size");
        editor.add_media_paths([PathBuf::from("source-size.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(video_track, TimelineMediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        editor.set_media_metadata(
            1,
            MediaMetadata {
                width: Some(3840),
                height: Some(2160),
                frame_rate: Some(59.94),
                frame_rate_ratio: SourceFrameRate::new(60_000, 1_001),
                ..Default::default()
            },
        );
        let target = editor.playback_target().unwrap();
        assert_eq!(target.source_size, Some((3840, 2160)));
        assert_eq!(
            target.source_frame_rate,
            SourceFrameRate::new(60_000, 1_001)
        );
        editor.set_media_metadata(
            1,
            MediaMetadata {
                width: Some(0),
                height: Some(2160),
                frame_rate: Some(f64::NAN),
                ..Default::default()
            },
        );
        let target = editor.playback_target().unwrap();
        assert_eq!(target.source_size, None);
        assert_eq!(target.source_frame_rate, None);
    }

    #[test]
    fn compositor_uv_mapping_respects_letterbox_content_crop_and_bilingual_sizing_labels() {
        let rect = decoded_content_uv_rect(PixelSize::new(4, 3), PixelSize::new(16, 9));
        assert!((rect.left() - 0.125).abs() < f32::EPSILON);
        assert!((rect.right() - 0.875).abs() < f32::EPSILON);
        assert_eq!(
            sizing_mode_label(Language::English, nle_timeline::ClipSizingMode::Fit),
            "Fit"
        );
        assert_eq!(
            sizing_mode_label(Language::Japanese, nle_timeline::ClipSizingMode::Fit),
            "フィット"
        );
        assert!(
            sizing_mode_tooltip(Language::English, nle_timeline::ClipSizingMode::Fill)
                .contains("Cover")
        );
        assert!((crop_edge_max(0.9) - 0.099).abs() < 0.0001);
        assert_eq!(crop_edge_max(2.0), 0.0);
        assert_eq!(project_aspect_label((1920, 1080)), "16:9");
        assert_eq!(project_aspect_label((1080, 1920)), "9:16");
    }

    #[test]
    fn viewer_mesh_applies_compositor_rotation_crop_and_letterboxed_texture_mapping() {
        let mut editor = EditorState::new(Language::English, "Compositor mesh");
        editor.set_project_canvas_size(400, 300);
        editor.add_media_paths([PathBuf::from("mesh.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let clip = editor
            .timeline
            .insert_clip(video_track, TimelineMediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        editor
            .timeline
            .set_clip_transform(
                clip,
                nle_timeline::ClipTransform {
                    sizing_mode: nle_timeline::ClipSizingMode::Stretch,
                    rotation_degrees: 90.0,
                    crop_left: 0.25,
                    ..Default::default()
                },
            )
            .unwrap();
        editor.set_media_metadata(
            1,
            MediaMetadata {
                width: Some(400),
                height: Some(300),
                ..Default::default()
            },
        );
        let texture = egui::TextureId::Managed(95);
        editor.set_monitor_frame_for_layer(0, texture, 1600, 900, Some(1), Some(Tick(0)));
        let context = egui::Context::default();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 640.0))),
                ..Default::default()
            },
            |ui| viewer(ui, &mut editor),
        );
        let mesh = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Mesh(mesh) if mesh.texture_id == texture => Some(mesh),
                _ => None,
            })
            .expect("composited mesh");
        assert_eq!(mesh.vertices.len(), 4);
        assert!((mesh.vertices[0].uv.x - 0.3125).abs() < 0.0001);
        assert!((mesh.vertices[1].uv.x - 0.875).abs() < 0.0001);
        assert!((mesh.vertices[0].pos.x - mesh.vertices[1].pos.x).abs() < 0.0001);
        assert_ne!(mesh.vertices[0].pos.y, mesh.vertices[1].pos.y);
    }

    #[test]
    fn complete_transform_round_trips_and_reset_has_single_undo_redo_steps() {
        let mut editor = EditorState::new(Language::English, "Transform history");
        editor.add_media_paths([PathBuf::from("transform.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let clip_id = editor
            .timeline
            .insert_clip(video_track, TimelineMediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let transform = nle_timeline::ClipTransform {
            opacity: 0.63,
            scale_x: 1.7,
            scale_y: 0.8,
            pos_x: -0.35,
            pos_y: 0.42,
            flip_h: true,
            flip_v: true,
            rotation_degrees: 37.0,
            anchor_x: 0.2,
            anchor_y: 0.75,
            crop_left: 0.11,
            crop_right: 0.19,
            crop_top: 0.07,
            crop_bottom: 0.13,
            sizing_mode: nle_timeline::ClipSizingMode::Original,
        };
        apply_track_header_edit(&mut editor, |timeline| {
            timeline.set_clip_transform(clip_id, transform)
        });
        assert_eq!(editor.timeline.clip(clip_id).unwrap().transform, transform);
        let snapshot = editor.snapshot();
        let mut restored =
            EditorState::restore(Language::English, "Transform history", snapshot).unwrap();
        assert_eq!(
            restored.timeline.clip(clip_id).unwrap().transform,
            transform
        );

        apply_track_header_edit(&mut restored, |timeline| {
            timeline.set_clip_transform(clip_id, nle_timeline::ClipTransform::default())
        });
        assert_eq!(
            restored.timeline.clip(clip_id).unwrap().transform,
            nle_timeline::ClipTransform::default()
        );
        assert!(restored.undo_timeline());
        assert_eq!(
            restored.timeline.clip(clip_id).unwrap().transform,
            transform
        );
        assert!(restored.redo_timeline());
        assert_eq!(
            restored.timeline.clip(clip_id).unwrap().transform,
            nle_timeline::ClipTransform::default()
        );
    }

    #[test]
    fn typed_inspector_scrub_commits_once_on_enter() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Inspector scrub history");
        editor.add_media_paths([PathBuf::from("transform.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        editor.history = EditorUndoStack::default();
        editor.pending_history = None;

        let response = inspector_opacity_input_frame(&context, &mut editor, clip_id, Vec::new());
        let center = response.rect.center();
        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![egui::Event::PointerButton {
                pos: center,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![egui::Event::Text("75".to_owned())],
        );
        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().transform.opacity,
            1.0,
            "typing must not create intermediate timeline edits"
        );
        assert!(editor.history.timeline.is_empty());

        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        inspector_opacity_input_frame(&context, &mut editor, clip_id, Vec::new());

        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().transform.opacity,
            0.75
        );
        assert_eq!(editor.history.timeline.len(), 1);
        assert!(editor.undo_timeline());
        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().transform.opacity,
            1.0
        );
        assert!(!editor.undo_timeline());
    }

    #[test]
    fn dragged_inspector_scrub_updates_live_and_undoes_once() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Inspector drag history");
        editor.add_media_paths([PathBuf::from("transform.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        editor.history = EditorUndoStack::default();
        editor.pending_history = None;

        let response = inspector_opacity_input_frame(&context, &mut editor, clip_id, Vec::new());
        let press = response.rect.center();
        let target = press - Vec2::new(40.0, 0.0);
        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![
                egui::Event::PointerMoved(press),
                egui::Event::PointerButton {
                    pos: press,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![egui::Event::PointerMoved(target)],
        );

        let dragged_opacity = editor.timeline.clip(clip_id).unwrap().transform.opacity;
        assert!(
            dragged_opacity < 1.0,
            "horizontal drag must update the live transform"
        );
        assert!(editor.pending_history.is_some());
        assert!(editor.history.timeline.is_empty());

        inspector_opacity_input_frame(
            &context,
            &mut editor,
            clip_id,
            vec![egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        assert_eq!(editor.history.timeline.len(), 1);
        assert!(editor.pending_history.is_none());
        assert!(editor.undo_timeline());
        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().transform.opacity,
            1.0
        );
        assert!(!editor.undo_timeline());
    }

    #[test]
    fn quick_export_accepts_transforms_and_four_layers_but_blocks_unsupported_render_features() {
        let mut editor = EditorState::new(Language::English, "Export truth");
        editor.add_media_paths([
            PathBuf::from("base.mp4"),
            PathBuf::from("upper.mp4"),
            PathBuf::from("third.mp4"),
            PathBuf::from("fourth.mp4"),
            PathBuf::from("fifth.mp4"),
            PathBuf::from("audio.wav"),
            PathBuf::from("still.png"),
        ]);
        let mut video_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while video_tracks.len() < 5 {
            video_tracks.push(editor.timeline.add_track(TrackKind::Video));
        }
        let base = editor
            .timeline
            .insert_clip(
                video_tracks[0],
                TimelineMediaId(1),
                Tick(0),
                Tick(100),
                Tick(0),
            )
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);

        let transform = nle_timeline::ClipTransform {
            rotation_degrees: 15.0,
            ..nle_timeline::ClipTransform::default()
        };
        editor.timeline.set_clip_transform(base, transform).unwrap();
        assert_eq!(editor.quick_export_block_message(), None);

        for (index, track) in video_tracks.iter().copied().enumerate().skip(1).take(3) {
            editor
                .timeline
                .insert_clip(
                    track,
                    TimelineMediaId(index as u32 + 1),
                    Tick(0),
                    Tick(100),
                    Tick(0),
                )
                .unwrap();
        }
        assert_eq!(editor.quick_export_block_message(), None);

        let fifth = editor
            .timeline
            .insert_clip(
                video_tracks[4],
                TimelineMediaId(5),
                Tick(0),
                Tick(100),
                Tick(0),
            )
            .unwrap();
        assert!(
            editor
                .quick_export_block_message()
                .unwrap()
                .contains("four visible video layers")
        );
        editor
            .timeline
            .set_clip_enabled(fifth, false, false)
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);
        editor
            .timeline
            .set_clip_enabled(fifth, true, false)
            .unwrap();
        editor
            .timeline
            .set_track_muted(video_tracks[4], true)
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);

        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let audio = editor
            .timeline
            .insert_clip(audio_track, TimelineMediaId(6), Tick(0), Tick(100), Tick(0))
            .unwrap();
        editor
            .timeline
            .set_clip_audio_effects(audio, vec![AudioEffect::HighPass { hz: 80 }])
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);
        editor
            .timeline
            .set_clip_audio_effects(
                audio,
                vec![AudioEffect::Bypassed(Box::new(AudioEffect::Limiter))],
            )
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);
        editor
            .timeline
            .set_clip_audio_effects(audio, vec![AudioEffect::Limiter])
            .unwrap();
        assert!(
            editor
                .quick_export_block_message()
                .unwrap()
                .contains("audio effects")
        );
        editor
            .timeline
            .set_clip_enabled(audio, false, false)
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);
        editor
            .timeline
            .set_track_audio_effects(audio_track, vec![AudioEffect::Limiter])
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);
        editor
            .timeline
            .set_clip_enabled(audio, true, false)
            .unwrap();
        assert!(
            editor
                .quick_export_block_message()
                .unwrap()
                .contains("audio effects")
        );
        editor.timeline.set_track_muted(audio_track, true).unwrap();
        assert_eq!(editor.quick_export_block_message(), None);

        editor
            .timeline
            .insert_clip(
                video_tracks[0],
                TimelineMediaId(7),
                Tick(200),
                Tick(100),
                Tick(0),
            )
            .unwrap();
        assert_eq!(editor.quick_export_block_message(), None);
    }

    #[test]
    fn still_images_place_on_v1_for_add_insert_and_overwrite_without_linked_audio() {
        let mut editor = EditorState::new(Language::English, "Still placement");
        editor.add_media_paths([PathBuf::from("base.mp4"), PathBuf::from("still.png")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;

        editor.selected_media = Some(2);
        assert!(editor.add_selected_to_timeline());
        let added = editor.selected_timeline_clip.unwrap();
        let added_clip = editor.timeline.clip(added).unwrap();
        assert_eq!(added_clip.track_id, video_track);
        assert_eq!(added_clip.duration, DEFAULT_STILL_IMAGE_DURATION);
        assert_eq!(added_clip.link_id, None);
        assert!(editor.provisional_clip_ids.is_empty());
        assert!(
            editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.id == audio_track)
                .unwrap()
                .clips
                .is_empty()
        );

        assert!(editor.insert_media_at(2, Tick(6_000_000)));
        let inserted = editor.selected_timeline_clip.unwrap();
        assert_eq!(
            editor.timeline.clip(inserted).unwrap().track_id,
            video_track
        );
        assert_eq!(editor.timeline.clip(inserted).unwrap().link_id, None);

        let before_overwrite = editor.timeline.snapshot();
        assert!(editor.overwrite_media_at(2, Tick(1_000_000)));
        let overwritten = editor.selected_timeline_clip.unwrap();
        assert_eq!(
            editor.timeline.clip(overwritten).unwrap().track_id,
            video_track
        );
        assert_eq!(editor.timeline.clip(overwritten).unwrap().link_id, None);
        assert!(
            editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.id == audio_track)
                .unwrap()
                .clips
                .is_empty()
        );
        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.snapshot(), before_overwrite);
        assert!(editor.redo_timeline());
        assert!(editor.timeline.clip(overwritten).is_some());
    }

    #[test]
    fn toolbar_insert_overwrite_and_replace_accept_stills_without_touching_audio() {
        for mode in [
            EditorEditMode::Insert,
            EditorEditMode::Overwrite,
            EditorEditMode::Replace,
        ] {
            let mut editor = EditorState::new(Language::English, "Still toolbar edit");
            editor.add_media_paths([PathBuf::from("base.mp4"), PathBuf::from("still.png")]);
            editor.selected_media = Some(1);
            assert!(editor.add_selected_to_timeline());
            let before = editor.timeline.snapshot();
            editor.selected_media = Some(2);
            editor.set_playhead(Tick(1_000_000));

            assert!(editor.edit_selected_at_playhead(mode));
            assert!(editor.timeline.tracks.iter().any(|track| {
                track.kind == TrackKind::Video
                    && track
                        .clips
                        .iter()
                        .any(|clip| clip.media == TimelineMediaId(2))
            }));
            assert!(editor.timeline.tracks.iter().all(|track| {
                track.kind != TrackKind::Audio
                    || track
                        .clips
                        .iter()
                        .all(|clip| clip.media != TimelineMediaId(2))
            }));
            assert!(editor.undo_timeline());
            assert_eq!(editor.timeline.snapshot(), before);
            assert!(editor.redo_timeline());
        }
    }

    #[test]
    fn still_playback_keeps_decode_tick_frozen_while_effect_time_advances() {
        let mut editor = EditorState::new(Language::English, "Animated still");
        editor.add_media_paths([PathBuf::from("still.png")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        editor
            .timeline
            .slip_clip(clip_id, Tick(2_000_000), false)
            .unwrap();
        let effect_id = VideoEffectId(1);
        editor
            .timeline
            .set_clip_video_effects(
                clip_id,
                vec![VideoEffectNode {
                    id: effect_id,
                    enabled: true,
                    kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
                }],
            )
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Brightness,
                Tick(2_000_000),
                0.0,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        editor
            .timeline
            .set_color_keyframe(
                clip_id,
                effect_id,
                ColorParameter::Brightness,
                Tick(3_000_000),
                0.4,
                KeyframeInterpolation::Linear,
            )
            .unwrap();

        editor.set_playhead(Tick(0));
        let start_decode_tick = editor.playback_target().unwrap().decode_tick;
        editor.set_playhead(Tick(500_000));
        let middle = editor.playback_target().unwrap();
        assert_eq!(start_decode_tick, Tick(0));
        assert_eq!(middle.decode_tick, Tick(0));
        assert_eq!(middle.source_frame_rate, None);
        assert_eq!(middle.source_tick, Tick(2_500_000));
        let nle_timeline::EvaluatedVideoEffect::BrightnessContrast(correction) =
            middle.video_effects.active()[0]
        else {
            panic!("expected the basic correction operation");
        };
        assert!((correction.brightness - 0.2).abs() < 0.0001);
    }

    #[test]
    fn still_analysis_never_clamps_a_user_extended_clip_to_default_duration() {
        let mut editor = EditorState::new(Language::English, "Extended still");
        editor.add_media_paths([PathBuf::from("still.png")]);
        assert!(editor.add_selected_to_timeline());
        let clip_id = editor.selected_timeline_clip.unwrap();
        editor
            .timeline
            .trim_end(clip_id, Tick(5_000_000), false, false)
            .unwrap();

        editor.set_media_metadata(
            1,
            MediaMetadata {
                duration_seconds: Some(5.0),
                width: Some(640),
                height: Some(360),
                ..Default::default()
            },
        );
        editor.set_video_strip(
            1,
            5,
            egui::TextureId::Managed(5),
            VideoStripLayout {
                duration: DEFAULT_STILL_IMAGE_DURATION,
                frame_count: 1,
                columns: 1,
                rows: 1,
                frame_width: 320,
                frame_height: 180,
            },
        );

        assert_eq!(
            editor.timeline.clip(clip_id).unwrap().duration,
            Tick(10_000_000)
        );
        assert_eq!(editor.media[0].duration, None);
    }

    #[test]
    fn viewer_composites_four_preview_layers_bottom_to_top() {
        let mut editor = EditorState::new(Language::English, "Four viewer layers");
        editor.add_media_paths([
            PathBuf::from("layer-1.mp4"),
            PathBuf::from("layer-2.mp4"),
            PathBuf::from("layer-3.mp4"),
            PathBuf::from("layer-4.mp4"),
        ]);
        let mut video_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        video_tracks.push(editor.timeline.add_track(TrackKind::Video));
        for (index, track) in video_tracks.into_iter().enumerate() {
            let clip = editor
                .timeline
                .insert_clip(
                    track,
                    TimelineMediaId(index as u32 + 1),
                    Tick(0),
                    Tick(100),
                    Tick(0),
                )
                .unwrap();
            editor
                .timeline
                .set_clip_transform(
                    clip,
                    nle_timeline::ClipTransform {
                        opacity: (index as f32 + 1.0) / 4.0,
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        editor.set_playhead(Tick(25));
        let textures = [
            egui::TextureId::Managed(71),
            egui::TextureId::Managed(72),
            egui::TextureId::Managed(73),
            egui::TextureId::Managed(74),
        ];
        for (layer, texture) in textures.into_iter().enumerate() {
            editor.set_monitor_frame_for_layer(
                layer,
                texture,
                640,
                360,
                Some(layer as u32 + 1),
                Some(Tick(25)),
            );
        }

        let context = egui::Context::default();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                ..Default::default()
            },
            |ui| viewer(ui, &mut editor),
        );
        let painted = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Mesh(mesh) if textures.contains(&mesh.texture_id) => {
                    Some((mesh.texture_id, mesh.vertices[0].color.a()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(painted.len(), PREVIEW_VIDEO_LAYER_COUNT);
        assert_eq!(
            painted
                .iter()
                .map(|(texture, _)| *texture)
                .collect::<Vec<_>>(),
            textures
        );
        assert_eq!(
            painted.iter().map(|(_, alpha)| *alpha).collect::<Vec<_>>(),
            [64, 128, 191, 255]
        );
    }

    #[test]
    fn missing_base_layer_does_not_reuse_the_top_compatibility_frame() {
        let mut editor = EditorState::new(Language::English, "Same-media layers");
        editor.add_media_paths([PathBuf::from("shared.mp4")]);
        let video_tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        for track in video_tracks {
            editor
                .timeline
                .insert_clip(track, TimelineMediaId(1), Tick(0), Tick(100), Tick(0))
                .unwrap();
        }
        editor.set_playhead(Tick(25));
        assert_eq!(editor.playback_targets().count(), 2);

        let top_texture = egui::TextureId::Managed(46);
        editor.set_monitor_frame_for_layer(1, top_texture, 640, 360, Some(1), Some(Tick(25)));
        assert_eq!(editor.monitor.unwrap().texture, top_texture);
        assert!(editor.monitor_frame_for_layer(0).is_none());

        let context = egui::Context::default();
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                ..Default::default()
            },
            |ui| viewer(ui, &mut editor),
        );
        let painted = output
            .shapes
            .iter()
            .filter(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::Shape::Mesh(mesh) if mesh.texture_id == top_texture
                )
            })
            .count();
        assert_eq!(
            painted, 1,
            "the ready upper texture must not fill a missing base slot"
        );
    }

    #[test]
    fn singular_monitor_api_remains_the_first_preview_layer() {
        let mut editor = EditorState::new(Language::English, "Compatibility");
        let texture = egui::TextureId::Managed(43);
        editor.set_monitor_frame_for_source(texture, 640, 360, Some(1), Some(Tick(30)));

        assert_eq!(editor.monitor.unwrap().texture, texture);
        assert_eq!(editor.monitor_frame_for_layer(0).unwrap().texture, texture);
        assert!(editor.monitor_frame_for_layer(1).is_none());
        editor.reset_monitor();
        assert!(editor.monitor.is_none());
        assert!(editor.monitor_frame_for_layer(0).is_none());
    }

    #[test]
    fn playback_target_holds_last_frame_at_project_rate_timeline_end() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        editor.add_selected_to_timeline();
        editor.set_playhead(Tick(15_000_000));
        assert_eq!(
            editor.playback_target().unwrap().source_tick,
            Tick(14_966_667)
        );
    }

    #[test]
    fn video_fades_drive_viewer_opacity_to_black_with_curve_shaping() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        editor.add_selected_to_timeline();
        let video = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .id;
        editor
            .timeline
            .set_fade_duration(video, FadeEdge::In, Tick(4_000_000))
            .unwrap();
        editor
            .timeline
            .set_fade_duration(video, FadeEdge::Out, Tick(3_000_000))
            .unwrap();

        editor.set_playhead(Tick(0));
        assert_eq!(editor.playback_target().unwrap().opacity, 0.0);
        editor.set_playhead(Tick(2_000_000));
        assert!(
            (editor.playback_target().unwrap().opacity - video_fade_opacity(0.5)).abs() < 0.001
        );
        editor.set_playhead(Tick(6_000_000));
        assert_eq!(editor.playback_target().unwrap().opacity, 1.0);
        editor.set_playhead(Tick(13_500_000));
        assert!(
            (editor.playback_target().unwrap().opacity - video_fade_opacity(0.5)).abs() < 0.001
        );
        editor.set_playhead(Tick(14_500_000));
        assert_eq!(editor.playback_target().unwrap().opacity, 0.0);
        editor.set_playhead(Tick(15_000_000));
        assert_eq!(editor.playback_target().unwrap().opacity, 0.0);

        assert!(
            (fade_envelope_value(
                nle_timeline::Fade {
                    duration: Tick(1),
                    curve: 1.0
                },
                0.5
            ) - 0.75)
                .abs()
                < 0.001
        );
        assert!(
            (fade_envelope_value(
                nle_timeline::Fade {
                    duration: Tick(1),
                    curve: -1.0
                },
                0.5
            ) - 0.25)
                .abs()
                < 0.001
        );
    }

    #[test]
    fn viewer_image_command_applies_fade_opacity_over_black() {
        fn rendered_image_alpha(editor: &mut EditorState, texture: egui::TextureId) -> u8 {
            let context = egui::Context::default();
            let output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1_000.0, 640.0))),
                    ..Default::default()
                },
                |ui| viewer(ui, editor),
            );
            output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Mesh(mesh) if mesh.texture_id == texture => {
                        mesh.vertices.first().map(|vertex| vertex.color.a())
                    }
                    _ => None,
                })
                .expect("viewer image mesh")
        }

        let mut editor = EditorState::new(Language::English, "Viewer fade");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        editor.add_selected_to_timeline();
        let target = editor.playback_target().unwrap();
        let video = target.clip_id;
        let media = target.media_id;
        let source_tick = target.source_tick;
        let texture = egui::TextureId::Managed(99);
        editor.set_monitor_frame_for_source(texture, 640, 360, Some(media), Some(source_tick));
        editor
            .timeline
            .set_fade_duration(video, FadeEdge::In, Tick(4_000_000))
            .unwrap();

        assert_eq!(rendered_image_alpha(&mut editor, texture), 0);
        editor
            .timeline
            .set_fade_duration(video, FadeEdge::In, Tick(0))
            .unwrap();
        assert_eq!(rendered_image_alpha(&mut editor, texture), 255);
    }

    #[test]
    fn viewer_holds_latest_frame_for_the_active_media_while_decode_catches_up() {
        let tagged = MonitorFrame {
            texture: egui::TextureId::Managed(3),
            width: 640,
            height: 360,
            media_id: Some(1),
            source_tick: Some(Tick(1_000_000)),
        };
        assert!(monitor_frame_matches_target(
            tagged,
            Some((1, Tick(1_100_000), 0.25)),
        ));
        assert!(!monitor_frame_matches_target(
            tagged,
            Some((2, Tick(1_100_000), 0.25)),
        ));
        assert!(monitor_frame_matches_target(
            tagged,
            Some((1, Tick(2_000_000), 0.25)),
        ));
        assert!(!monitor_frame_matches_target(tagged, None));
    }

    #[test]
    fn analysis_meter_uses_live_output_peaks_and_stop_state() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        editor.add_selected_to_timeline();
        editor
            .set_waveform_with_audio_info(
                1,
                Tick(15_000_000),
                vec![(-0.5, 0.5); 16],
                Some(48_000),
                Some(2),
            )
            .unwrap();
        let audio = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .clips[0]
            .id;
        editor.timeline.set_audio_gain(audio, -6.0).unwrap();
        editor.set_playhead(Tick(1_000_000));
        editor.playing = true;
        editor.set_audio_meter_levels(0.25, 0.5);
        let (levels, channels) = active_audio_levels(&editor).unwrap();
        assert_eq!(channels, 2);
        assert_eq!(levels, (0.25, 0.5));
        editor.playing = false;
        assert_eq!(active_audio_levels(&editor), Some(((0.0, 0.0), 2)));
        editor.set_audio_meter_levels(f32::NAN, -4.0);
        assert_eq!(editor.audio_meter_levels, (0.0, 1.0));
    }

    #[test]
    fn right_sidebar_selection_is_runtime_only_and_routes_tabs() {
        let mut editor = EditorState::new(Language::English, "Right sidebar tabs");
        let snapshot = editor.snapshot();
        let generation = editor.durable_generation();

        assert_eq!(RightSidebarTab::ALL.len(), 5);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Inspector);
        select_right_sidebar_tab(&mut editor, RightSidebarTab::Audio);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Audio);
        select_right_sidebar_tab(&mut editor, RightSidebarTab::Color);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Color);
        select_right_sidebar_tab(&mut editor, RightSidebarTab::Effects);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Effects);
        select_right_sidebar_tab(&mut editor, RightSidebarTab::Media);
        assert_eq!(editor.right_sidebar_tab, RightSidebarTab::Media);
        assert_eq!(editor.snapshot(), snapshot);
        assert_eq!(editor.durable_generation(), generation);

        let restored = EditorState::restore(Language::English, "Right sidebar tabs", snapshot)
            .expect("snapshot remains independent of the selected right sidebar tab");
        assert_eq!(restored.right_sidebar_tab, RightSidebarTab::Inspector);
    }

    #[test]
    fn right_sidebar_tabs_click_and_keep_long_metadata_within_minimum_width() {
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Right sidebar layout");
        let long_value = "metadata-value-".repeat(24);
        editor.add_media_paths([PathBuf::from(format!(
            "C:/a-very-long-media-library-path/{long_value}/clip-with-a-very-long-name.mp4"
        ))]);
        editor.set_media_metadata(
            1,
            MediaMetadata {
                container: Some(long_value.clone()),
                streams: vec![MediaStreamMetadata {
                    index: 42,
                    kind: Some(long_value.clone()),
                    codec: Some(long_value),
                    time_base: Some("1/12345678901234567890".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        editor.right_sidebar_tab = RightSidebarTab::Media;

        let mut panel_rect = Rect::NOTHING;
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(220.0, 320.0))),
                ..Default::default()
            },
            |ui| {
                details(ui, &mut editor);
                panel_rect = ui.min_rect();
            },
        );
        assert!(
            panel_rect.right() <= 220.0,
            "panel expanded to {panel_rect:?}"
        );
        assert!(
            output
                .shapes
                .iter()
                .all(|shape| shape.clip_rect.right() <= 220.0)
        );

        for (events, expected) in [
            (
                vec![
                    egui::Event::PointerMoved(Pos2::new(110.0, 20.0)),
                    egui::Event::PointerButton {
                        pos: Pos2::new(110.0, 20.0),
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                RightSidebarTab::Media,
            ),
            (
                vec![egui::Event::PointerButton {
                    pos: Pos2::new(110.0, 20.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                RightSidebarTab::Color,
            ),
        ] {
            let _ = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(220.0, 320.0))),
                    events,
                    ..Default::default()
                },
                |ui| details(ui, &mut editor),
            );
            assert_eq!(editor.right_sidebar_tab, expected);
        }

        assert_ne!(
            RightSidebarTab::Inspector.scroll_id(),
            RightSidebarTab::Audio.scroll_id()
        );
        assert_ne!(
            RightSidebarTab::Audio.scroll_id(),
            RightSidebarTab::Color.scroll_id()
        );
        assert_ne!(
            RightSidebarTab::Color.scroll_id(),
            RightSidebarTab::Media.scroll_id()
        );
    }

    #[test]
    fn analysis_panel_distinguishes_idle_unplaced_pending_ready_live_and_offline() {
        let mut editor = EditorState::new(Language::English, "Analysis states");
        assert_eq!(
            analysis_panel_state(&editor, editor.selected_media),
            AnalysisPanelState::NoSelection
        );
        assert_eq!(
            analysis_panel_status(Language::English, AnalysisPanelState::NoSelection, false),
            "No selection"
        );
        assert_eq!(
            analysis_panel_status(Language::Japanese, AnalysisPanelState::NoSelection, false),
            "未選択"
        );

        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert_eq!(
            analysis_panel_state(&editor, editor.selected_media),
            AnalysisPanelState::AwaitingPlacement
        );
        assert_eq!(
            analysis_panel_status(
                Language::Japanese,
                AnalysisPanelState::AwaitingPlacement,
                false,
            ),
            "未解析"
        );

        assert!(editor.add_selected_to_timeline());
        assert_eq!(
            analysis_panel_state(&editor, editor.selected_media),
            AnalysisPanelState::Analyzing
        );

        editor.set_media_metadata(1, MediaMetadata::default());
        editor
            .set_waveform(1, Tick(15_000_000), vec![(-0.2, 0.2)])
            .unwrap();
        assert_eq!(
            analysis_panel_state(&editor, editor.selected_media),
            AnalysisPanelState::Ready
        );
        assert_eq!(
            analysis_panel_status(Language::English, AnalysisPanelState::Ready, true),
            "Live"
        );

        editor.set_media_error(1, "source unavailable");
        assert_eq!(
            analysis_panel_state(&editor, editor.selected_media),
            AnalysisPanelState::Offline
        );
        assert_eq!(
            analysis_panel_status(Language::Japanese, AnalysisPanelState::Offline, false),
            "オフライン"
        );
    }

    #[test]
    fn probed_metadata_is_available_to_the_analysis_panel() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        editor.set_media_metadata(
            1,
            MediaMetadata {
                video_codec: Some("h264".into()),
                width: Some(1920),
                height: Some(1088),
                frame_rate: Some(24.0),
                audio_codec: Some("aac".into()),
                sample_rate: Some(48_000),
                channels: Some(2),
                streams: vec![MediaStreamMetadata {
                    index: 0,
                    kind: Some("video".into()),
                    codec: Some("h264".into()),
                    duration_seconds: Some(15.0),
                    time_base: Some("1/12288".into()),
                    width: Some(1920),
                    height: Some(1088),
                    frame_rate: Some(24.0),
                    ..Default::default()
                }],
                ..MediaMetadata::default()
            },
        );
        let metadata = editor.media_metadata.get(&1).unwrap();
        assert_eq!(format_video_metadata(Some(metadata)), "H264 · 1920×1088");
        assert_eq!(
            format_audio_metadata(Some(metadata), None),
            "AAC · 48.0 kHz · Stereo"
        );
        assert_eq!(
            format_stream_metadata(&metadata.streams[0]),
            "video · H264 · 1920×1088 · 24.000 fps · duration 15.000 s · time base 1/12288"
        );
    }

    #[test]
    fn monitor_reset_drops_frame_and_status() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.set_monitor_frame(egui::TextureId::Managed(7), 1920, 1080);
        assert!(editor.monitor.is_some());
        assert_eq!(editor.monitor_status, MonitorStatus::Ready);
        editor.reset_monitor();
        assert!(editor.monitor.is_none());
        assert_eq!(editor.monitor_status, MonitorStatus::Empty);
    }

    #[test]
    fn scrub_gap_clears_the_live_monitor_to_black() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.set_monitor_frame(egui::TextureId::Managed(9), 640, 360);
        editor.clear_monitor_for_gap();
        assert!(editor.monitor.is_none());
        assert_eq!(editor.monitor_status, MonitorStatus::Empty);
    }

    #[test]
    fn full_monitor_size_uses_physical_viewer_pixels_and_only_caps_at_8k() {
        assert_eq!(quantize_monitor_size(641.0, 361.0, 1.0), (640, 352));
        assert_eq!(quantize_monitor_size(1_920.0, 1_080.0, 1.0), (1_920, 1_072));
        assert_eq!(quantize_monitor_size(1_920.0, 1_080.0, 2.0), (3_840, 2_160));
        assert_eq!(quantize_monitor_size(4_000.0, 3_000.0, 1.0), (4_000, 2_992));
        assert_eq!(
            quantize_monitor_size(10_000.0, 6_000.0, 1.0),
            (8_192, 4_912)
        );
        assert_eq!(quantize_monitor_size(1.0, 1.0, 1.0), (16, 16));
        assert_eq!(quantize_monitor_size(640.0, 360.0, f32::NAN), (640, 352));
    }

    #[test]
    fn preview_quality_scales_the_quantized_monitor_decode_hint() {
        let mut editor = EditorState::new(Language::English, "Preview quality");
        editor.monitor_decode_size = quantize_monitor_size(641.0, 361.0, 1.0);
        assert_eq!(editor.monitor_decode_size_hint(), (640, 352));

        for (quality, expected) in [
            (PreviewQuality::Full, (640, 352)),
            (PreviewQuality::Half, (320, 176)),
            (PreviewQuality::Quarter, (160, 88)),
            (PreviewQuality::Eighth, (80, 44)),
        ] {
            editor.set_preview_quality(quality);
            assert_eq!(editor.monitor_decode_size_hint(), expected);
        }

        editor.monitor_decode_size = (16, 16);
        assert_eq!(editor.monitor_decode_size_hint(), (16, 16));
        editor.monitor_decode_size = (32, 160);
        assert_eq!(editor.monitor_decode_size_hint(), (16, 80));
    }

    #[test]
    fn scrub_decode_size_uses_the_selected_moving_playback_quality() {
        let mut editor = EditorState::new(Language::English, "Scrub decode quality");
        editor.monitor_decode_size = quantize_monitor_size(641.0, 361.0, 1.0);

        for (quality, expected) in [
            (PreviewQuality::Full, (640, 352)),
            (PreviewQuality::Half, (320, 176)),
            (PreviewQuality::Quarter, (160, 88)),
        ] {
            editor.set_preview_quality(quality);
            assert_eq!(editor.monitor_scrub_decode_size_hint(), expected);
        }

        assert!(editor.set_preview_quality(PreviewQuality::Eighth));
        assert_eq!(editor.monitor_scrub_decode_size_hint(), (80, 44));

        assert!(editor.set_preview_quality(PreviewQuality::Auto));
        assert_eq!(editor.resolved_preview_quality(), PreviewQuality::Full);
        assert_eq!(editor.monitor_scrub_decode_size_hint(), (640, 352));
    }

    #[test]
    fn scrub_decode_size_preserves_tiny_axis_floor_and_aspect_behavior() {
        let mut editor = EditorState::new(Language::English, "Tiny scrub decode quality");
        assert!(editor.set_preview_quality(PreviewQuality::Quarter));
        editor.monitor_decode_size = (16, 160);
        assert_eq!(editor.monitor_scrub_decode_size_hint(), (16, 160));

        editor.monitor_decode_size = (32, 160);
        assert_eq!(editor.monitor_scrub_decode_size_hint(), (16, 80));
    }

    #[test]
    fn auto_preview_quality_is_runtime_only_and_keeps_retained_frames() {
        let mut editor = EditorState::new(Language::English, "Preview quality");
        let generation = editor.durable_generation();
        assert!(editor.set_preview_quality(PreviewQuality::Auto));
        editor.set_monitor_frame(egui::TextureId::Managed(41), 640, 360);
        assert!(editor.set_auto_preview_quality(PreviewQuality::Half));
        assert_eq!(editor.preview_quality(), PreviewQuality::Auto);
        assert_eq!(editor.resolved_preview_quality(), PreviewQuality::Half);
        assert_eq!(editor.durable_generation(), generation + 1);
        assert_eq!(
            editor.monitor.unwrap().texture,
            egui::TextureId::Managed(41)
        );
        assert!(!editor.set_auto_preview_quality(PreviewQuality::Auto));
        assert_eq!(editor.durable_generation(), generation + 1);

        assert!(editor.set_preview_quality(PreviewQuality::Quarter));
        assert_eq!(editor.durable_generation(), generation + 2);
        assert_eq!(editor.resolved_preview_quality(), PreviewQuality::Quarter);
        assert!(editor.set_auto_preview_quality(PreviewQuality::Eighth));
        assert_eq!(editor.resolved_preview_quality(), PreviewQuality::Quarter);
        assert_eq!(editor.durable_generation(), generation + 2);
    }

    #[test]
    fn playback_quality_preferences_persist_and_legacy_fields_default_to_full() {
        let mut editor = EditorState::new(Language::English, "Preview quality");
        assert!(editor.set_preview_quality(PreviewQuality::Eighth));
        assert!(editor.set_paused_preview_quality(PreviewQuality::Half));
        assert!(editor.set_high_quality_playback(false));
        let snapshot = editor.snapshot();
        assert_eq!(snapshot.view.preview_quality, PreviewQuality::Eighth);
        assert_eq!(snapshot.view.paused_preview_quality, PreviewQuality::Half);
        assert!(!snapshot.view.high_quality_playback);
        let restored =
            EditorState::restore(Language::English, "Preview quality", snapshot).unwrap();
        assert_eq!(restored.preview_quality(), PreviewQuality::Eighth);
        assert_eq!(restored.paused_preview_quality(), PreviewQuality::Half);
        assert!(!restored.high_quality_playback());

        let mut legacy_json = serde_json::to_value(editor.snapshot()).unwrap();
        legacy_json["view"]
            .as_object_mut()
            .unwrap()
            .remove("preview_quality");
        legacy_json["view"]
            .as_object_mut()
            .unwrap()
            .remove("preview_quality_is_explicit");
        legacy_json["view"]
            .as_object_mut()
            .unwrap()
            .remove("paused_preview_quality");
        legacy_json["view"]
            .as_object_mut()
            .unwrap()
            .remove("high_quality_playback");
        let legacy: EditorProjectSnapshot = serde_json::from_value(legacy_json).unwrap();
        let restored = EditorState::restore(Language::English, "Preview quality", legacy).unwrap();
        assert_eq!(restored.preview_quality(), PreviewQuality::Full);
        assert_eq!(restored.resolved_preview_quality(), PreviewQuality::Full);
        assert_eq!(restored.paused_preview_quality(), PreviewQuality::Full);
        assert_eq!(
            restored.resolved_paused_preview_quality(),
            PreviewQuality::Full
        );
        assert!(restored.high_quality_playback());

        let mut legacy_auto_json = serde_json::to_value(editor.snapshot()).unwrap();
        legacy_auto_json["view"]["preview_quality"] = serde_json::json!("Auto");
        legacy_auto_json["view"]
            .as_object_mut()
            .unwrap()
            .remove("preview_quality_is_explicit");
        let legacy_auto: EditorProjectSnapshot = serde_json::from_value(legacy_auto_json).unwrap();
        let restored = EditorState::restore(Language::English, "Legacy Auto", legacy_auto).unwrap();
        assert_eq!(restored.preview_quality(), PreviewQuality::Full);

        let mut explicit_auto = EditorState::new(Language::English, "Explicit Auto");
        assert!(explicit_auto.set_preview_quality(PreviewQuality::Auto));
        let restored =
            EditorState::restore(Language::English, "Explicit Auto", explicit_auto.snapshot())
                .unwrap();
        assert_eq!(restored.preview_quality(), PreviewQuality::Auto);
    }

    #[test]
    fn paused_quality_and_high_quality_playback_are_independent_durable_preferences() {
        let mut editor = EditorState::new(Language::English, "Playback preferences");
        editor.monitor_decode_size = quantize_monitor_size(641.0, 361.0, 1.0);
        let generation = editor.durable_generation();

        assert_eq!(editor.preview_quality(), PreviewQuality::Full);
        assert_eq!(editor.paused_preview_quality(), PreviewQuality::Full);
        assert!(editor.high_quality_playback());
        assert!(editor.set_preview_quality(PreviewQuality::Half));
        assert!(editor.set_paused_preview_quality(PreviewQuality::Quarter));
        assert!(editor.set_high_quality_playback(false));
        assert_eq!(editor.durable_generation(), generation + 3);
        assert_eq!(editor.monitor_playback_decode_size_hint(), (320, 176));
        assert_eq!(editor.monitor_scrub_decode_size_hint(), (320, 176));
        assert_eq!(editor.monitor_paused_decode_size_hint(), (160, 88));
        assert!(!editor.set_preview_quality(PreviewQuality::Half));
        assert!(!editor.set_paused_preview_quality(PreviewQuality::Quarter));
        assert!(!editor.set_high_quality_playback(false));
        assert_eq!(editor.durable_generation(), generation + 3);
    }

    #[test]
    fn paused_auto_restores_safely_without_being_a_menu_choice() {
        let mut editor = EditorState::new(Language::English, "Legacy paused auto");
        assert!(editor.set_paused_preview_quality(PreviewQuality::Auto));
        assert!(editor.set_auto_preview_quality(PreviewQuality::Half));
        assert_eq!(
            editor.resolved_paused_preview_quality(),
            PreviewQuality::Half
        );
        assert!(!preview_quality_menu_choices(true).contains(&PreviewQuality::Auto));
        assert!(preview_quality_menu_choices(false).contains(&PreviewQuality::Auto));
    }

    #[test]
    fn preview_quality_header_displays_auto_with_its_runtime_fraction() {
        assert_eq!(
            preview_quality_display(
                Language::English,
                PreviewQuality::Auto,
                PreviewQuality::Half
            ),
            "Auto · 1/2"
        );
        assert_eq!(
            preview_quality_display(
                Language::Japanese,
                PreviewQuality::Auto,
                PreviewQuality::Quarter
            ),
            "自動 · 1/4"
        );
        assert_eq!(
            preview_quality_option_label(Language::English, PreviewQuality::Eighth),
            "Eighth · 1/8"
        );
    }

    #[test]
    fn video_strip_selects_the_nearest_sample_for_scrubbing() {
        let layout = VideoStripLayout {
            duration: Tick(8_000_000),
            frame_count: 8,
            columns: 3,
            rows: 3,
            frame_width: 128,
            frame_height: 72,
        };
        assert_eq!(video_strip_frame_index(layout, Tick(0)), 0);
        assert_eq!(video_strip_frame_index(layout, Tick(1_490_000)), 1);
        assert_eq!(video_strip_frame_index(layout, Tick(1_510_000)), 2);
        assert_eq!(video_strip_frame_index(layout, Tick(7_999_999)), 7);
        assert_eq!(video_strip_frame_index(layout, Tick(99_000_000)), 7);
    }

    #[test]
    fn video_strip_uvs_stay_inside_the_row_major_grid() {
        let layout = VideoStripLayout {
            duration: Tick(5_000_000),
            frame_count: 5,
            columns: 3,
            rows: 2,
            frame_width: 128,
            frame_height: 72,
        };
        let first = video_strip_frame_uv(layout, 0);
        assert_eq!(first.min, Pos2::new(0.0, 0.0));
        assert_eq!(first.max, Pos2::new(1.0 / 3.0, 0.5));
        let last = video_strip_frame_uv(layout, 4);
        assert_eq!(last.min, Pos2::new(1.0 / 3.0, 0.5));
        assert_eq!(last.max, Pos2::new(2.0 / 3.0, 1.0));
        let clamped = video_strip_frame_uv(layout, 99);
        assert_eq!(clamped, last);
        for uv in [first, last, clamped] {
            assert!(uv.min.x >= 0.0 && uv.min.y >= 0.0);
            assert!(uv.max.x <= 1.0 && uv.max.y <= 1.0);
        }
    }

    #[test]
    fn project_frame_rates_format_step_and_hold_on_absolute_boundaries() {
        for (frame_rate, first_boundary, one_second_timecode, final_frame) in [
            (
                ProjectFrameRate::new(24, 1).unwrap(),
                Tick(41_667),
                "00:00:01:00",
                Tick(14_958_334),
            ),
            (
                ProjectFrameRate::new(30, 1).unwrap(),
                Tick(33_334),
                "00:00:01:00",
                Tick(14_966_667),
            ),
            (
                ProjectFrameRate::new(25, 1).unwrap(),
                Tick(40_000),
                "00:00:01:00",
                Tick(14_960_000),
            ),
            (
                ProjectFrameRate::new(30_000, 1_001).unwrap(),
                Tick(33_367),
                "00:00:00:29",
                Tick(14_981_634),
            ),
        ] {
            assert_eq!(
                format_timecode_at_frame_rate(first_boundary, frame_rate),
                "00:00:00:01"
            );
            assert_eq!(
                format_timecode_at_frame_rate(Tick(1_000_000), frame_rate),
                one_second_timecode
            );

            let mut editor =
                EditorState::new_with_frame_rate(Language::English, "Test", frame_rate);
            editor.add_media_paths([PathBuf::from("clip.mp4")]);
            assert!(editor.add_selected_to_timeline());
            editor.next_frame();
            assert_eq!(editor.playhead, first_boundary);
            editor.next_frame();
            editor.previous_frame();
            assert_eq!(editor.playhead, first_boundary);
            editor.set_playhead(editor.timeline_end());
            assert_eq!(editor.playback_target().unwrap().source_tick, final_frame);
        }
    }

    #[test]
    fn source_phase_drives_end_hold_and_dynamic_trim_boundaries() {
        for (frame_rate, held_source_tick, trim_back, trim_forward) in [
            (
                ProjectFrameRate::new(30, 1).unwrap(),
                Tick(15_000_000),
                Tick(-10_000),
                Tick(23_334),
            ),
            (
                ProjectFrameRate::new(30_000, 1_001).unwrap(),
                Tick(14_981_634),
                Tick(-28_366),
                Tick(5_000),
            ),
        ] {
            let mut editor =
                EditorState::new_with_frame_rate(Language::English, "Source phase", frame_rate);
            editor.add_media_paths([PathBuf::from("clip.mp4")]);
            assert!(editor.add_selected_to_timeline());
            let clip_id = editor.selected_timeline_clip.unwrap();
            editor
                .timeline
                .slip_clip(clip_id, Tick(10_000), false)
                .unwrap();

            editor.set_playhead(editor.timeline_end());
            let held = editor.playback_target().unwrap().source_tick;
            assert_eq!(held, held_source_tick);
            assert_eq!(editor.quantize_tick_to_frame_start(held), held);
            assert_eq!(editor.dynamic_trim_delta(clip_id, -1), Some(trim_back));
            assert_eq!(editor.dynamic_trim_delta(clip_id, 1), Some(trim_forward));
        }
    }

    #[test]
    fn fps_aware_restore_keeps_document_owned_frame_rate_out_of_snapshot() {
        assert!(ProjectFrameRate::new(0, 1).is_err());
        assert!(ProjectFrameRate::new(30, 0).is_err());
        let frame_rate = ProjectFrameRate::new(30_000, 1_001).unwrap();
        let editor = EditorState::new_with_frame_rate(Language::English, "NTSC", frame_rate);
        let json = serde_json::to_string(&editor.snapshot()).unwrap();
        assert!(!json.contains("frame_rate"));

        let restored = EditorState::restore_with_frame_rate(
            Language::English,
            "NTSC",
            serde_json::from_str(&json).unwrap(),
            frame_rate,
        )
        .unwrap();
        assert_eq!(restored.frame_rate, frame_rate);
        assert_eq!(
            EditorState::new(Language::English, "Default").frame_rate,
            ProjectFrameRate::DEFAULT
        );
        assert_eq!(
            EditorState::restore(Language::English, "Default", editor.snapshot())
                .unwrap()
                .frame_rate,
            ProjectFrameRate::DEFAULT
        );
    }

    #[test]
    fn project_snapshot_json_round_trip_preserves_durable_editor_state() {
        let mut editor = EditorState::new(Language::Japanese, "作品");
        editor.add_media_paths([PathBuf::from("clip.mp4"), PathBuf::from("music.wav")]);
        assert!(editor.add_selected_to_timeline());
        let clip = editor.selected_timeline_clip.unwrap();
        let track = editor.timeline.tracks[0].id;
        editor.set_playhead(Tick(1_000_000));
        editor.set_zoom_handles(0.2, 0.8);
        editor.media_pool_width = 310.0;
        editor.analysis_width = 410.0;
        editor.timeline_height = 440.0;
        editor.timeline_height_is_default = false;
        editor.timeline_scroll_y = 72.0;
        editor.track_heights.insert(track, 86.0);

        let snapshot = editor.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let decoded: EditorProjectSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, snapshot);
        let restored = EditorState::restore(Language::Japanese, "作品", decoded).unwrap();

        assert_eq!(restored.snapshot(), snapshot);
        assert_eq!(restored.selected_timeline_clip, Some(clip));
        assert_eq!(restored.media[0].kind, MediaKind::Video);
    }

    #[test]
    fn native_title_create_edit_round_trip_delete_and_undo_are_durable() {
        let mut editor = EditorState::new(Language::Japanese, "タイトル作品");
        editor.set_playhead(Tick(2_000_000));
        assert!(editor.add_title_at_playhead());
        let title_id = editor.selected_title.unwrap();

        let before = editor.timeline_history_checkpoint();
        let mut title = editor.timeline.title(title_id).unwrap().clone();
        title.start = Tick(2_000_000);
        title.text = "Maelstrom\n日本語".to_owned();
        title.alignment = TitleAlignment::Right;
        title.position_x = 0.72;
        title.opacity = 0.65;
        title.fade_in = Tick(250_000);
        editor
            .timeline
            .replace_title(title_id, title.clone())
            .unwrap();
        editor.record_timeline_history(before);

        assert_eq!(editor.timeline_end(), Tick(7_000_000));
        let snapshot = editor.snapshot();
        let restored = EditorState::restore(
            Language::Japanese,
            "タイトル作品",
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.selected_title, Some(title_id));
        assert_eq!(restored.timeline.title(title_id), Some(&title));

        assert!(editor.delete_selected_timeline_clip());
        assert!(editor.timeline.title(title_id).is_none());
        assert!(editor.undo_timeline());
        assert_eq!(editor.timeline.title(title_id), Some(&title));
        assert!(editor.redo_timeline());
        assert!(editor.timeline.title(title_id).is_none());
    }

    #[test]
    fn legacy_editor_view_without_title_selection_restores_cleanly() {
        let editor = EditorState::new(Language::English, "Legacy view");
        let mut value = serde_json::to_value(editor.snapshot()).unwrap();
        value["view"]
            .as_object_mut()
            .unwrap()
            .remove("selected_title");
        let snapshot: EditorProjectSnapshot = serde_json::from_value(value).unwrap();
        let restored = EditorState::restore(Language::English, "Legacy view", snapshot).unwrap();
        assert_eq!(restored.selected_title, None);
        assert!(restored.timeline.titles().is_empty());
    }

    #[test]
    fn project_restore_canonicalizes_reordered_media_before_slot_indexed_playback() {
        let mut editor = EditorState::new(Language::English, "Reordered media");
        editor.add_media_paths([PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]);
        editor.selected_media = Some(1);
        assert!(editor.add_selected_to_timeline());
        editor.selected_media = Some(2);
        assert!(editor.add_selected_to_timeline());

        let mut snapshot = editor.snapshot();
        snapshot.media.reverse();
        let mut restored =
            EditorState::restore(Language::English, "Reordered media", snapshot).unwrap();
        assert_eq!(
            restored
                .media
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        restored.set_playhead(Tick(1_000_000));
        assert_eq!(
            restored.playback_target().unwrap().path,
            Path::new("first.mp4")
        );
        restored.set_playhead(Tick(16_000_000));
        assert_eq!(
            restored.playback_target().unwrap().path,
            Path::new("second.mp4")
        );
    }

    #[test]
    fn project_snapshot_rejects_old_and_corrupt_references() {
        let editor = EditorState::new(Language::English, "Test");
        let mut old = editor.snapshot();
        old.version = EDITOR_PROJECT_SNAPSHOT_VERSION - 1;
        assert!(matches!(
            EditorState::restore(Language::English, "Test", old),
            Err(EditorRestoreError::UnsupportedVersion(0))
        ));

        let mut corrupt = editor.snapshot();
        corrupt.media = vec![
            EditorMediaSnapshot {
                id: 7,
                path: PathBuf::from("one.mp4"),
                duration: None,
            },
            EditorMediaSnapshot {
                id: 7,
                path: PathBuf::from("two.mp4"),
                duration: None,
            },
        ];
        assert!(matches!(
            EditorState::restore(Language::English, "Test", corrupt),
            Err(EditorRestoreError::DuplicateMediaId(7))
        ));

        let mut exhausted = editor.snapshot();
        exhausted.media.push(EditorMediaSnapshot {
            id: MediaId::MAX,
            path: PathBuf::from("exhausted.mp4"),
            duration: None,
        });
        assert!(matches!(
            EditorState::restore(Language::English, "Test", exhausted),
            Err(EditorRestoreError::InvalidMediaId(MediaId::MAX))
        ));

        let mut sparse = editor.snapshot();
        sparse.media.push(EditorMediaSnapshot {
            id: 7,
            path: PathBuf::from("sparse.mp4"),
            duration: None,
        });
        assert!(matches!(
            EditorState::restore(Language::English, "Test", sparse),
            Err(EditorRestoreError::NonContiguousMediaId {
                expected: 1,
                actual: 7,
            })
        ));

        let mut dangling = editor.snapshot();
        let track_id = dangling.timeline.tracks[0].id;
        dangling.timeline.tracks[0].clips.push(nle_timeline::Clip {
            id: ClipId(99),
            media: TimelineMediaId(99),
            track_id,
            link_id: None,
            enabled: true,
            start: Tick(0),
            duration: Tick(1),
            source_in: Tick(0),
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new(),
            transform: nle_timeline::ClipTransform::default(),
            fade_in: Default::default(),
            fade_out: Default::default(),
        });
        assert!(matches!(
            EditorState::restore(Language::English, "Test", dangling),
            Err(EditorRestoreError::UnknownTimelineMedia(99))
        ));
    }

    #[test]
    fn project_snapshot_excludes_runtime_state_and_resets_transport() {
        let mut editor = EditorState::new(Language::English, "Test");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        editor.set_monitor_frame(egui::TextureId::Managed(4), 640, 360);
        editor.set_video_strip(
            1,
            5,
            egui::TextureId::Managed(5),
            VideoStripLayout {
                duration: Tick(15_000_000),
                frame_count: 6,
                columns: 3,
                rows: 2,
                frame_width: 128,
                frame_height: 72,
            },
        );
        editor.set_waveform_error(1, "decode failed");
        editor.set_media_error(1, "file missing");
        editor.set_media_decoder_backend(1, "Intel Quick Sync");
        editor.force_software_decode = true;
        editor.set_export_completed(PathBuf::from("runtime-export.mp4"));
        editor.start_playback();
        editor.emit(EditorAction::ChooseMediaFiles);

        let json = serde_json::to_string(&editor.snapshot()).unwrap();
        assert!(!json.contains("decode failed"));
        assert!(!json.contains("file missing"));
        assert!(!json.contains("Intel Quick Sync"));
        assert!(!json.contains("runtime-export.mp4"));
        let restored: EditorProjectSnapshot = serde_json::from_str(&json).unwrap();
        let restored = EditorState::restore(Language::English, "Test", restored).unwrap();
        assert!(!restored.playing);
        assert!(restored.monitor.is_none());
        assert_eq!(restored.monitor_status, MonitorStatus::Empty);
        assert!(restored.waveforms.is_empty());
        assert!(restored.waveform_errors.is_empty());
        assert!(restored.media_errors.is_empty());
        assert!(restored.media_decoder_backends.is_empty());
        assert!(!restored.force_software_decode);
        assert!(restored.video_strips.is_empty());
        assert!(restored.action.is_none());
        assert_eq!(restored.export_status, EditorExportStatus::Idle);
    }

    #[test]
    fn active_preview_diagnostics_are_fixed_per_layer_and_clear_with_monitor() {
        let mut editor = EditorState::new(Language::English, "Preview diagnostics");
        let diagnostic = ActivePreviewDiagnostic::new(
            7,
            ActivePreviewSourceKind::OriginalSource,
            Some(ActivePreviewDecoderBackend::IntelQuickSync),
            Some(ActivePreviewFallbackReason::HardwareDecodeFailed),
            PreviewQuality::Auto,
            PreviewQuality::Half,
            [960, 540],
        );
        assert!(editor.set_active_preview_diagnostic_for_layer(3, diagnostic));
        assert_eq!(
            editor.active_preview_diagnostic_for_layer(3),
            Some(diagnostic)
        );
        assert!(!editor.set_active_preview_diagnostic_for_layer(4, diagnostic));
        assert!(editor.active_preview_diagnostic_for_layer(4).is_none());
        assert!(editor.clear_active_preview_diagnostic_for_layer(3));
        assert!(editor.active_preview_diagnostic_for_layer(3).is_none());
        assert!(editor.set_active_preview_diagnostic_for_layer(0, diagnostic));
        editor.reset_monitor();
        assert!(editor.active_preview_diagnostic_for_layer(0).is_none());
    }

    #[test]
    fn active_preview_diagnostic_labels_are_localized_without_misnaming_scrub_preview() {
        assert_eq!(
            active_preview_source_label(
                Language::English,
                ActivePreviewSourceKind::InternalScrubPreview
            ),
            "Internal scrub preview"
        );
        assert_eq!(
            active_preview_source_label(
                Language::Japanese,
                ActivePreviewSourceKind::InternalScrubPreview
            ),
            "内部スクラブプレビュー"
        );
        assert_eq!(
            active_preview_decoder_label(
                Language::Japanese,
                ActivePreviewDecoderBackend::WindowsD3d11va
            ),
            "Windows D3D11VA"
        );
        assert_eq!(
            active_preview_fallback_label(
                Language::Japanese,
                ActivePreviewFallbackReason::ForcedSoftware
            ),
            "ソフトウェアを強制"
        );
        let unobserved = ActivePreviewDiagnostic::new(
            1,
            ActivePreviewSourceKind::InternalScrubPreview,
            None,
            None,
            PreviewQuality::Auto,
            PreviewQuality::Quarter,
            [160, 90],
        );
        assert_eq!(
            active_preview_fallback_status_label(Language::English, unobserved),
            "Not observed"
        );
        let hardware = ActivePreviewDiagnostic {
            source_kind: ActivePreviewSourceKind::OriginalSource,
            decoder_backend: Some(ActivePreviewDecoderBackend::WindowsD3d11va),
            ..unobserved
        };
        assert_eq!(
            active_preview_fallback_status_label(Language::Japanese, hardware),
            "不要"
        );
    }

    #[test]
    fn active_preview_diagnostics_are_excluded_from_project_snapshots() {
        let mut editor = EditorState::new(Language::English, "Preview diagnostics");
        let diagnostic = ActivePreviewDiagnostic::new(
            42,
            ActivePreviewSourceKind::InternalScrubPreview,
            None,
            None,
            PreviewQuality::Quarter,
            PreviewQuality::Quarter,
            [480, 270],
        );
        assert!(editor.set_active_preview_diagnostic_for_layer(1, diagnostic));
        let json = serde_json::to_string(&editor.snapshot()).unwrap();
        assert!(!json.contains("InternalScrubPreview"));
        assert!(!json.contains("480"));
        let restored = EditorState::restore(
            Language::English,
            "Preview diagnostics",
            serde_json::from_str(&json).unwrap(),
        )
        .unwrap();
        assert!(restored.active_preview_diagnostic_for_layer(1).is_none());
    }
}
