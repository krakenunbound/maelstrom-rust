//! UI-, decode-, and GPU-independent timeline source of truth.
//!
//! Tracks own compact, ordered clip arrays. All edit methods keep the
//! non-overlap invariant so UI queries can later use binary search.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use serde::{Deserialize, Serialize};

/// Project-locked timeline unit. A project supplies its timebase elsewhere.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize,
)]
pub struct Tick(pub i64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct TrackId(pub u32);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct ClipId(pub u32);

/// Stable identity for a durable video transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct TransitionId(pub u32);

/// Stable identity for a durable audio transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct AudioTransitionId(pub u32);

/// Stable identity for a durable title overlay.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct TitleId(pub u32);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum TitleAlignment {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TitleColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl TitleColor {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    pub const fn white() -> Self {
        Self::rgba(255, 255, 255, 255)
    }
    pub const fn black() -> Self {
        Self::rgba(0, 0, 0, 255)
    }
}

impl Default for TitleColor {
    fn default() -> Self {
        Self::white()
    }
}

/// A renderer-independent text overlay positioned in normalized output coordinates.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TitleOverlay {
    pub id: TitleId,
    pub start: Tick,
    pub duration: Tick,
    pub text: String,
    pub font_size: f32,
    pub alignment: TitleAlignment,
    pub position_x: f32,
    pub position_y: f32,
    pub fill: TitleColor,
    pub outline_color: TitleColor,
    pub outline_width: f32,
    pub shadow_color: TitleColor,
    pub shadow_offset_x: f32,
    pub shadow_offset_y: f32,
    pub shadow_blur: f32,
    pub opacity: f32,
    pub fade_in: Tick,
    pub fade_out: Tick,
    pub enabled: bool,
    pub z_order: i32,
}

impl TitleOverlay {
    fn new(id: TitleId, start: Tick, duration: Tick, text: String) -> Self {
        Self {
            id,
            start,
            duration,
            text,
            font_size: 48.0,
            alignment: TitleAlignment::Center,
            position_x: 0.5,
            position_y: 0.82,
            fill: TitleColor::white(),
            outline_color: TitleColor::black(),
            outline_width: 0.0,
            shadow_color: TitleColor::black(),
            shadow_offset_x: 2.0,
            shadow_offset_y: 2.0,
            shadow_blur: 2.0,
            opacity: 1.0,
            fade_in: Tick(0),
            fade_out: Tick(0),
            enabled: true,
            z_order: 0,
        }
    }
}

impl Default for TitleOverlay {
    fn default() -> Self {
        Self::new(TitleId(1), Tick(0), Tick(5_000_000), "Title".to_owned())
    }
}

/// Stable identity for a video effect node inside a clip.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct VideoEffectId(pub u32);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct MediaId(pub u32);

/// Identifies clips that were created as the audio/video parts of one source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
pub struct LinkId(pub u32);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FadeEdge {
    In,
    Out,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum VideoTransitionKind {
    #[default]
    CrossDissolve,
    /// A gamma-shaped dissolve with a brighter, more film-like roll-in than a
    /// standard linear cross dissolve.
    FilmDissolve,
    DipToBlack,
    DipToWhite,
    WipeLeft,
    WipeRight,
    WipeUp,
    WipeDown,
    SlideFromLeft,
    SlideFromRight,
    SlideFromTop,
    SlideFromBottom,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum AudioTransitionKind {
    #[default]
    EqualPowerCrossfade,
}

/// A transition centered on the exact cut between two adjacent video clips.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VideoTransition {
    pub id: TransitionId,
    pub track_id: TrackId,
    pub left_clip: ClipId,
    pub right_clip: ClipId,
    pub duration: Tick,
    pub curve: f32,
    #[serde(default)]
    pub kind: VideoTransitionKind,
}

/// An equal-power crossfade centered on the exact cut between adjacent audio clips.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AudioTransition {
    pub id: AudioTransitionId,
    pub track_id: TrackId,
    pub left_clip: ClipId,
    pub right_clip: ClipId,
    pub duration: Tick,
    #[serde(default)]
    pub kind: AudioTransitionKind,
}

/// Durable audio processors shared by the video editor and Undertow.  Values
/// deliberately mirror Undertow's preset list; renderer-specific filter
/// graphs live outside the timeline source of truth.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum AudioEffect {
    /// Retains an effect and all of its settings while removing it from the
    /// active signal path. One wrapper is intentional: nested bypasses are
    /// rejected during setter and snapshot validation.
    Bypassed(Box<AudioEffect>),
    Normalize,
    Chorus,
    DeEsser,
    DeHummer,
    Delay,
    DialogueProcessor,
    Distortion,
    LowPass {
        hz: u32,
    },
    Lfe {
        hz: u32,
    },
    HighPass {
        hz: u32,
    },
    Eq {
        hz: u32,
        db: f32,
    },
    Compressor,
    Limiter,
    Modulation,
    MultibandCompressor,
    NoiseReduction,
    Pitch {
        semitones: f32,
    },
    Echo,
    Reverb,
    Flanger,
    SoftClipper,
    StereoFixer,
    StereoWidth {
        width: f32,
    },
    Tremolo,
    VocalChannel,
}

impl AudioEffect {
    pub const MIN_FILTER_HZ: u32 = 20;
    /// Preview and export share this ceiling so filter controls remain below
    /// Nyquist for the editor's 44.1/48 kHz professional audio contract.
    /// `MAX_FILTER_HZ` remains broader for backward-compatible project reads.
    pub const MAX_RENDER_FILTER_HZ: u32 = 20_000;
    pub const MAX_FILTER_HZ: u32 = 96_000;
    pub const MIN_EQ_DB: f32 = -24.0;
    pub const MAX_EQ_DB: f32 = 24.0;
    pub const MIN_PITCH_SEMITONES: f32 = -24.0;
    pub const MAX_PITCH_SEMITONES: f32 = 24.0;
    pub const MIN_STEREO_WIDTH: f32 = 0.0;
    pub const MAX_STEREO_WIDTH: f32 = 2.0;

    pub fn effective_filter_hz(hz: u32) -> u32 {
        hz.clamp(Self::MIN_FILTER_HZ, Self::MAX_RENDER_FILTER_HZ)
    }

    /// Returns true when this rack entry is retained but inactive.
    pub fn is_bypassed(&self) -> bool {
        matches!(self, Self::Bypassed(_))
    }

    /// Returns the active processor, or `None` for a bypassed rack entry.
    pub fn enabled(&self) -> Option<&Self> {
        match self {
            Self::Bypassed(_) => None,
            effect => Some(effect),
        }
    }

    /// Whether this active processor has an exact bundled-FFmpeg lowering.
    /// Bypassed entries are intentionally not export-supported because they
    /// do not participate in the signal path.
    pub fn is_export_supported(&self) -> bool {
        matches!(
            self,
            Self::HighPass { .. }
                | Self::LowPass { .. }
                | Self::Eq { .. }
                | Self::StereoWidth { .. }
        )
    }

    pub fn is_valid(&self) -> bool {
        match self {
            Self::Bypassed(effect) => !effect.is_bypassed() && effect.is_valid(),
            Self::LowPass { hz } | Self::Lfe { hz } | Self::HighPass { hz } => {
                (Self::MIN_FILTER_HZ..=Self::MAX_FILTER_HZ).contains(hz)
            }
            Self::Eq { hz, db } => {
                (Self::MIN_FILTER_HZ..=Self::MAX_FILTER_HZ).contains(hz)
                    && db.is_finite()
                    && (Self::MIN_EQ_DB..=Self::MAX_EQ_DB).contains(db)
            }
            Self::Pitch { semitones } => {
                semitones.is_finite()
                    && (Self::MIN_PITCH_SEMITONES..=Self::MAX_PITCH_SEMITONES).contains(semitones)
            }
            Self::StereoWidth { width } => {
                width.is_finite()
                    && (Self::MIN_STEREO_WIDTH..=Self::MAX_STEREO_WIDTH).contains(width)
            }
            _ => true,
        }
    }
}

/// Clip-owned effects remain bounded so preview and export can use a fixed
/// evaluation buffer without allocating on every frame.
pub const MAX_VIDEO_EFFECTS_PER_CLIP: usize = 8;
/// Audio rack entries are bounded independently for each clip and audio
/// track. This keeps snapshot validation and signal-graph construction cheap.
pub const MAX_AUDIO_EFFECTS_PER_SCOPE: usize = 8;
pub const MAX_KEYFRAMES_PER_PARAMETER: usize = 256;
pub const MAX_COLOR_CURVE_POINTS: usize = 16;
/// One lookup entry per encoded 8-bit channel value, matching FFmpeg's RGB curve LUT.
#[cfg(test)]
const COLOR_CURVE_LUT_SAMPLES: usize = 256;
pub const MIN_BRIGHTNESS: f32 = -1.0;
pub const MAX_BRIGHTNESS: f32 = 1.0;
pub const MIN_CONTRAST: f32 = 0.0;
pub const MAX_CONTRAST: f32 = 4.0;
pub const MIN_TEMPERATURE: f32 = -1.0;
pub const MAX_TEMPERATURE: f32 = 1.0;
pub const MIN_TINT: f32 = -1.0;
pub const MAX_TINT: f32 = 1.0;
pub const MIN_SATURATION: f32 = 0.0;
pub const MAX_SATURATION: f32 = 2.0;
pub const MIN_EXPOSURE: f32 = -5.0;
pub const MAX_EXPOSURE: f32 = 5.0;
pub const MIN_HIGHLIGHTS: f32 = -1.0;
pub const MAX_HIGHLIGHTS: f32 = 1.0;
pub const MIN_SHADOWS: f32 = -1.0;
pub const MAX_SHADOWS: f32 = 1.0;
/// Near-white tonal adjustment. Zero is identity; values are encoded-sRGB offsets.
pub const MIN_WHITES: f32 = -1.0;
pub const MAX_WHITES: f32 = 1.0;
/// Near-black tonal adjustment. Zero is identity; values are encoded-sRGB offsets.
pub const MIN_BLACKS: f32 = -1.0;
pub const MAX_BLACKS: f32 = 1.0;
pub const MIN_VIGNETTE_AMOUNT: f32 = 0.0;
pub const MAX_VIGNETTE_AMOUNT: f32 = 1.0;
pub const MIN_VIGNETTE_MIDPOINT: f32 = 0.0;
pub const MAX_VIGNETTE_MIDPOINT: f32 = 0.95;
pub const MIN_VIGNETTE_FEATHER: f32 = 0.01;
pub const MAX_VIGNETTE_FEATHER: f32 = 1.0;
pub const MIN_VIGNETTE_CENTER: f32 = -1.0;
pub const MAX_VIGNETTE_CENTER: f32 = 1.0;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum KeyframeInterpolation {
    /// Interpolate from this key to the following key.
    #[default]
    Linear,
    /// Hold this key's value until the following key.
    Hold,
    /// Interpolate with zero velocity at both keys.
    Smooth,
    /// Interpolate slowly from this key, then accelerate toward the next key.
    EaseIn,
    /// Interpolate quickly from this key, then decelerate toward the next key.
    EaseOut,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScalarKeyframe {
    /// Absolute source tick, preserving animation through trim, slip, and razor.
    pub source_tick: Tick,
    pub value: f32,
    #[serde(default)]
    pub interpolation: KeyframeInterpolation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AnimatedScalar {
    pub value: f32,
    #[serde(default)]
    pub keyframes: Vec<ScalarKeyframe>,
}

impl AnimatedScalar {
    pub fn evaluate(&self, source_tick: Tick) -> f32 {
        let Some(first) = self.keyframes.first() else {
            return self.value;
        };
        if source_tick <= first.source_tick {
            return first.value;
        }
        let Some(last) = self.keyframes.last() else {
            return self.value;
        };
        if source_tick >= last.source_tick {
            return last.value;
        }
        let right = self
            .keyframes
            .partition_point(|key| key.source_tick < source_tick);
        let left = &self.keyframes[right - 1];
        let right = &self.keyframes[right];
        match left.interpolation {
            KeyframeInterpolation::Hold => left.value,
            KeyframeInterpolation::Linear
            | KeyframeInterpolation::Smooth
            | KeyframeInterpolation::EaseIn
            | KeyframeInterpolation::EaseOut => {
                let distance = (right.source_tick.0 - left.source_tick.0) as f32;
                let position = (source_tick.0 - left.source_tick.0) as f32 / distance;
                let position = match left.interpolation {
                    KeyframeInterpolation::Linear => position,
                    KeyframeInterpolation::Smooth => position * position * (3.0 - 2.0 * position),
                    KeyframeInterpolation::EaseIn => position * position,
                    KeyframeInterpolation::EaseOut => 1.0 - (1.0 - position) * (1.0 - position),
                    KeyframeInterpolation::Hold => unreachable!("hold is handled above"),
                };
                left.value + (right.value - left.value) * position
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct CurvePoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ColorCurve {
    pub points: Vec<CurvePoint>,
}

impl Default for ColorCurve {
    fn default() -> Self {
        Self {
            points: vec![CurvePoint { x: 0.0, y: 0.0 }, CurvePoint { x: 1.0, y: 1.0 }],
        }
    }
}

impl ColorCurve {
    pub fn is_identity(&self) -> bool {
        self.points == ColorCurve::default().points
    }

    /// Samples the same clipped natural cubic used to build the viewer LUT.
    pub fn sample(&self, input: f32) -> f32 {
        natural_curve_sample(self, input)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct RgbCurves {
    #[serde(default)]
    pub master: ColorCurve,
    #[serde(default)]
    pub red: ColorCurve,
    #[serde(default)]
    pub green: ColorCurve,
    #[serde(default)]
    pub blue: ColorCurve,
}

impl RgbCurves {
    pub fn is_identity(&self) -> bool {
        self.master.is_identity()
            && self.red.is_identity()
            && self.green.is_identity()
            && self.blue.is_identity()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrightnessContrastEffect {
    #[serde(default = "default_brightness")]
    pub brightness: AnimatedScalar,
    #[serde(default = "default_contrast")]
    pub contrast: AnimatedScalar,
    #[serde(default = "default_brightness")]
    pub temperature: AnimatedScalar,
    #[serde(default = "default_brightness")]
    pub tint: AnimatedScalar,
    #[serde(default = "default_saturation")]
    pub saturation: AnimatedScalar,
    #[serde(default = "default_exposure")]
    pub exposure: AnimatedScalar,
    #[serde(default = "default_brightness")]
    pub highlights: AnimatedScalar,
    #[serde(default = "default_brightness")]
    pub shadows: AnimatedScalar,
    /// Near-white adjustment, kept separate from the broader Highlights control.
    #[serde(default = "default_brightness")]
    pub whites: AnimatedScalar,
    /// Near-black adjustment, kept separate from the broader Shadows control.
    #[serde(default = "default_brightness")]
    pub blacks: AnimatedScalar,
    #[serde(default)]
    pub curves: RgbCurves,
}

fn default_brightness() -> AnimatedScalar {
    AnimatedScalar {
        value: 0.0,
        keyframes: Vec::new(),
    }
}

fn default_contrast() -> AnimatedScalar {
    AnimatedScalar {
        value: 1.0,
        keyframes: Vec::new(),
    }
}

fn default_saturation() -> AnimatedScalar {
    AnimatedScalar {
        value: 1.0,
        keyframes: Vec::new(),
    }
}

fn default_exposure() -> AnimatedScalar {
    AnimatedScalar {
        value: 0.0,
        keyframes: Vec::new(),
    }
}

impl Default for BrightnessContrastEffect {
    fn default() -> Self {
        Self {
            brightness: default_brightness(),
            contrast: default_contrast(),
            temperature: default_brightness(),
            tint: default_brightness(),
            saturation: default_saturation(),
            exposure: default_exposure(),
            highlights: default_brightness(),
            shadows: default_brightness(),
            whites: default_brightness(),
            blacks: default_brightness(),
            curves: RgbCurves::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VignetteEffect {
    #[serde(default = "default_vignette_amount")]
    pub amount: AnimatedScalar,
    #[serde(default = "default_vignette_midpoint")]
    pub midpoint: AnimatedScalar,
    #[serde(default = "default_vignette_feather")]
    pub feather: AnimatedScalar,
    #[serde(default = "default_brightness")]
    pub center_x: AnimatedScalar,
    #[serde(default = "default_brightness")]
    pub center_y: AnimatedScalar,
}

fn default_vignette_amount() -> AnimatedScalar {
    AnimatedScalar {
        value: 0.35,
        keyframes: Vec::new(),
    }
}

fn default_vignette_midpoint() -> AnimatedScalar {
    AnimatedScalar {
        value: 0.45,
        keyframes: Vec::new(),
    }
}

fn default_vignette_feather() -> AnimatedScalar {
    AnimatedScalar {
        value: 0.5,
        keyframes: Vec::new(),
    }
}

impl Default for VignetteEffect {
    fn default() -> Self {
        Self {
            amount: default_vignette_amount(),
            midpoint: default_vignette_midpoint(),
            feather: default_vignette_feather(),
            center_x: default_brightness(),
            center_y: default_brightness(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Effects remain inline so the timeline evaluation path is allocation-free.
#[allow(clippy::large_enum_variant)]
pub enum VideoEffectKind {
    BrightnessContrast(BrightnessContrastEffect),
    Vignette(VignetteEffect),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VideoEffectNode {
    pub id: VideoEffectId,
    #[serde(default = "default_effect_enabled")]
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: VideoEffectKind,
}

fn default_effect_enabled() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorParameter {
    Brightness,
    Contrast,
    Temperature,
    Tint,
    Saturation,
    Exposure,
    Highlights,
    Shadows,
    Whites,
    Blacks,
    VignetteAmount,
    VignetteMidpoint,
    VignetteFeather,
    VignetteCenterX,
    VignetteCenterY,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvaluatedColorCurve {
    pub points: [CurvePoint; MAX_COLOR_CURVE_POINTS],
    pub count: u8,
}

impl Default for EvaluatedColorCurve {
    fn default() -> Self {
        let mut points = [CurvePoint { x: 0.0, y: 0.0 }; MAX_COLOR_CURVE_POINTS];
        points[1] = CurvePoint { x: 1.0, y: 1.0 };
        Self { points, count: 2 }
    }
}

impl From<&ColorCurve> for EvaluatedColorCurve {
    fn from(curve: &ColorCurve) -> Self {
        let mut evaluated = Self {
            count: curve.points.len().min(MAX_COLOR_CURVE_POINTS) as u8,
            ..Self::default()
        };
        evaluated.points[..usize::from(evaluated.count)]
            .copy_from_slice(&curve.points[..usize::from(evaluated.count)]);
        evaluated
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EvaluatedRgbCurves {
    pub master: EvaluatedColorCurve,
    pub red: EvaluatedColorCurve,
    pub green: EvaluatedColorCurve,
    pub blue: EvaluatedColorCurve,
}

impl From<&RgbCurves> for EvaluatedRgbCurves {
    fn from(curves: &RgbCurves) -> Self {
        Self {
            master: EvaluatedColorCurve::from(&curves.master),
            red: EvaluatedColorCurve::from(&curves.red),
            green: EvaluatedColorCurve::from(&curves.green),
            blue: EvaluatedColorCurve::from(&curves.blue),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvaluatedBrightnessContrast {
    pub brightness: f32,
    pub contrast: f32,
    pub temperature: f32,
    pub tint: f32,
    pub saturation: f32,
    pub exposure: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    /// Compact static control points; the renderer expands these directly into its GPU LUT.
    pub curves: EvaluatedRgbCurves,
}

impl Default for EvaluatedBrightnessContrast {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            temperature: 0.0,
            tint: 0.0,
            saturation: 1.0,
            exposure: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            curves: EvaluatedRgbCurves::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvaluatedVignette {
    pub amount: f32,
    pub midpoint: f32,
    pub feather: f32,
    pub center_x: f32,
    pub center_y: f32,
}

impl Default for EvaluatedVignette {
    fn default() -> Self {
        Self {
            amount: 0.35,
            midpoint: 0.45,
            feather: 0.5,
            center_x: 0.0,
            center_y: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
// The evaluated effect union is fixed-size and Copy by design for allocation-free frame plans.
#[allow(clippy::large_enum_variant)]
pub enum EvaluatedVideoEffect {
    BrightnessContrast(EvaluatedBrightnessContrast),
    Vignette(EvaluatedVignette),
}

impl Default for EvaluatedVideoEffect {
    fn default() -> Self {
        Self::BrightnessContrast(EvaluatedBrightnessContrast::default())
    }
}

/// Ordered, allocation-free video-effect operations evaluated for one frame.
///
/// The backing array is intentionally private: callers can only inspect the
/// initialized prefix through [`Self::active`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvaluatedVideoEffectStack {
    operations: [EvaluatedVideoEffect; MAX_VIDEO_EFFECTS_PER_CLIP],
    active_len: u8,
}

impl Default for EvaluatedVideoEffectStack {
    fn default() -> Self {
        Self {
            operations: [EvaluatedVideoEffect::default(); MAX_VIDEO_EFFECTS_PER_CLIP],
            active_len: 0,
        }
    }
}

impl EvaluatedVideoEffectStack {
    /// The enabled operations in their durable clip order.
    pub fn active(&self) -> &[EvaluatedVideoEffect] {
        &self.operations[..usize::from(self.active_len)]
    }

    pub fn len(&self) -> usize {
        usize::from(self.active_len)
    }

    pub fn is_empty(&self) -> bool {
        self.active_len == 0
    }

    fn push(&mut self, operation: EvaluatedVideoEffect) {
        let index = usize::from(self.active_len);
        debug_assert!(index < MAX_VIDEO_EFFECTS_PER_CLIP);
        if let Some(slot) = self.operations.get_mut(index) {
            *slot = operation;
            self.active_len += 1;
        }
    }
}

/// Destination lanes used by the first edit-toolbar operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditTarget {
    VideoAndAudio,
    VideoOnly,
    AudioOnly,
}

/// A neutral curve is linear. Negative and positive values bend in opposite
/// directions; rendering decides the exact curve implementation.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Fade {
    pub duration: Tick,
    pub curve: f32,
}

impl Default for Fade {
    fn default() -> Self {
        Self {
            duration: Tick(0),
            curve: 0.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Clip {
    pub id: ClipId,
    pub media: MediaId,
    /// The track containing this clip. Kept on the clip for ID-based clients.
    pub track_id: TrackId,
    pub link_id: Option<LinkId>,
    pub start: Tick,
    pub duration: Tick,
    pub source_in: Tick,
    /// Disabled clips remain placed on the timeline but are bypassed by media
    /// consumers. Older projects predate this control and therefore restore as
    /// enabled.
    #[serde(default = "default_clip_enabled")]
    pub enabled: bool,
    /// Only meaningful for audio clips. Values are clamped by `set_audio_gain`.
    #[serde(default)]
    pub gain_db: f32,
    /// Independent stereo trim used by Undertow before track gain and pan.
    #[serde(default)]
    pub gain_left_db: f32,
    #[serde(default)]
    pub gain_right_db: f32,
    #[serde(default)]
    pub effects: Vec<AudioEffect>,
    /// Ordered, clip-owned video effects. Legacy projects restore with none.
    #[serde(default)]
    pub video_effects: Vec<VideoEffectNode>,
    /// Viewer transform. Defaults are identity so legacy projects restore unchanged.
    #[serde(default)]
    pub transform: ClipTransform,
    pub fade_in: Fade,
    pub fade_out: Fade,
}

fn default_clip_enabled() -> bool {
    true
}

/// Preview transform for the monitor. Identity is scale 1, opacity 1, centered.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClipTransform {
    #[serde(default = "default_transform_opacity")]
    pub opacity: f32,
    #[serde(default = "default_transform_scale")]
    pub scale_x: f32,
    #[serde(default = "default_transform_scale")]
    pub scale_y: f32,
    #[serde(default)]
    pub pos_x: f32,
    #[serde(default)]
    pub pos_y: f32,
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    /// Clockwise screen-space rotation, normalized to [-180, 180) degrees.
    #[serde(default)]
    pub rotation_degrees: f32,
    /// Normalized pivot inside the post-sizing rectangle.
    #[serde(default = "default_transform_anchor")]
    pub anchor_x: f32,
    #[serde(default = "default_transform_anchor")]
    pub anchor_y: f32,
    /// Normalized source crop, applied before sizing and transform.
    #[serde(default)]
    pub crop_left: f32,
    #[serde(default)]
    pub crop_right: f32,
    #[serde(default)]
    pub crop_top: f32,
    #[serde(default)]
    pub crop_bottom: f32,
    #[serde(default)]
    pub sizing_mode: ClipSizingMode,
}

/// How a cropped source is sized in the project frame before clip scale.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum ClipSizingMode {
    /// Preserve aspect ratio and show the full source content.
    #[default]
    Fit,
    /// Preserve aspect ratio and cover the project frame.
    Fill,
    /// Independently scale each axis to the project frame.
    Stretch,
    /// Preserve cropped source pixels at a 1:1 size.
    Original,
}

fn default_transform_opacity() -> f32 {
    1.0
}

fn default_transform_scale() -> f32 {
    1.0
}

fn default_transform_anchor() -> f32 {
    0.5
}

impl Default for ClipTransform {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            scale_x: 1.0,
            scale_y: 1.0,
            pos_x: 0.0,
            pos_y: 0.0,
            flip_h: false,
            flip_v: false,
            rotation_degrees: 0.0,
            anchor_x: 0.5,
            anchor_y: 0.5,
            crop_left: 0.0,
            crop_right: 0.0,
            crop_top: 0.0,
            crop_bottom: 0.0,
            sizing_mode: ClipSizingMode::Fit,
        }
    }
}

impl ClipTransform {
    pub const MIN_OPACITY: f32 = 0.0;
    pub const MAX_OPACITY: f32 = 1.0;
    pub const MIN_SCALE: f32 = 0.05;
    pub const MAX_SCALE: f32 = 8.0;
    pub const MIN_POS: f32 = -2.0;
    pub const MAX_POS: f32 = 2.0;
    pub const MIN_ROTATION_DEGREES: f32 = -180.0;
    pub const MAX_ROTATION_DEGREES: f32 = 180.0;
    pub const MIN_ANCHOR: f32 = 0.0;
    pub const MAX_ANCHOR: f32 = 1.0;
    /// Leaves at least 0.1% of cropped content on each axis.
    pub const MAX_CROP_TOTAL: f32 = 0.999;

    pub fn clamped(self) -> Self {
        let mut result = Self {
            opacity: self.opacity.clamp(Self::MIN_OPACITY, Self::MAX_OPACITY),
            scale_x: self.scale_x.clamp(Self::MIN_SCALE, Self::MAX_SCALE),
            scale_y: self.scale_y.clamp(Self::MIN_SCALE, Self::MAX_SCALE),
            pos_x: self.pos_x.clamp(Self::MIN_POS, Self::MAX_POS),
            pos_y: self.pos_y.clamp(Self::MIN_POS, Self::MAX_POS),
            flip_h: self.flip_h,
            flip_v: self.flip_v,
            rotation_degrees: canonical_rotation_degrees(self.rotation_degrees),
            anchor_x: self.anchor_x.clamp(Self::MIN_ANCHOR, Self::MAX_ANCHOR),
            anchor_y: self.anchor_y.clamp(Self::MIN_ANCHOR, Self::MAX_ANCHOR),
            crop_left: self.crop_left.clamp(0.0, 1.0),
            crop_right: self.crop_right.clamp(0.0, 1.0),
            crop_top: self.crop_top.clamp(0.0, 1.0),
            crop_bottom: self.crop_bottom.clamp(0.0, 1.0),
            sizing_mode: self.sizing_mode,
        };
        clamp_crop_pair(&mut result.crop_left, &mut result.crop_right);
        clamp_crop_pair(&mut result.crop_top, &mut result.crop_bottom);
        result
    }

    pub fn is_finite(&self) -> bool {
        self.opacity.is_finite()
            && self.scale_x.is_finite()
            && self.scale_y.is_finite()
            && self.pos_x.is_finite()
            && self.pos_y.is_finite()
            && self.rotation_degrees.is_finite()
            && self.anchor_x.is_finite()
            && self.anchor_y.is_finite()
            && self.crop_left.is_finite()
            && self.crop_right.is_finite()
            && self.crop_top.is_finite()
            && self.crop_bottom.is_finite()
    }
}

fn canonical_rotation_degrees(degrees: f32) -> f32 {
    (degrees + 180.0).rem_euclid(360.0) - 180.0
}

fn clamp_crop_pair(first: &mut f32, second: &mut f32) {
    let total = *first + *second;
    if total > ClipTransform::MAX_CROP_TOTAL {
        let factor = ClipTransform::MAX_CROP_TOTAL / total;
        *first *= factor;
        *second *= factor;
    }
}

impl Clip {
    pub fn end(&self) -> Tick {
        Tick(self.start.0.saturating_add(self.duration.0))
    }

    /// Clip gain plus owning-track gain. Channel trim and pan stay separate.
    pub fn mix_gain_db(&self, track: &Track) -> f32 {
        (self.gain_db + track.gain_db).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
    }

    /// Evaluates every enabled video effect at an absolute source tick.
    pub fn evaluate_video_effects(&self, source_tick: Tick) -> EvaluatedVideoEffectStack {
        let mut evaluated = EvaluatedVideoEffectStack::default();
        for node in &self.video_effects {
            if !node.enabled {
                continue;
            }
            match &node.kind {
                VideoEffectKind::BrightnessContrast(effect) => {
                    evaluated.push(EvaluatedVideoEffect::BrightnessContrast(
                        EvaluatedBrightnessContrast {
                            brightness: effect.brightness.evaluate(source_tick),
                            contrast: effect.contrast.evaluate(source_tick),
                            temperature: effect.temperature.evaluate(source_tick),
                            tint: effect.tint.evaluate(source_tick),
                            saturation: effect.saturation.evaluate(source_tick),
                            exposure: effect.exposure.evaluate(source_tick),
                            highlights: effect.highlights.evaluate(source_tick),
                            shadows: effect.shadows.evaluate(source_tick),
                            whites: effect.whites.evaluate(source_tick),
                            blacks: effect.blacks.evaluate(source_tick),
                            curves: EvaluatedRgbCurves::from(&effect.curves),
                        },
                    ));
                }
                VideoEffectKind::Vignette(effect) => {
                    evaluated.push(EvaluatedVideoEffect::Vignette(EvaluatedVignette {
                        amount: effect.amount.evaluate(source_tick),
                        midpoint: effect.midpoint.evaluate(source_tick),
                        feather: effect.feather.evaluate(source_tick),
                        center_x: effect.center_x.evaluate(source_tick),
                        center_y: effect.center_y.evaluate(source_tick),
                    }));
                }
            }
        }
        evaluated
    }
}

fn normalize_animated_scalar(scalar: &mut AnimatedScalar, minimum: f32, maximum: f32) -> bool {
    if !scalar.value.is_finite() || scalar.keyframes.len() > MAX_KEYFRAMES_PER_PARAMETER {
        return false;
    }
    scalar.value = scalar.value.clamp(minimum, maximum);
    let mut previous_tick = None;
    for key in &mut scalar.keyframes {
        if key.source_tick.0 < 0
            || !key.value.is_finite()
            || previous_tick.is_some_and(|previous| previous >= key.source_tick)
        {
            return false;
        }
        key.value = key.value.clamp(minimum, maximum);
        previous_tick = Some(key.source_tick);
    }
    true
}

fn normalize_color_curve(curve: &ColorCurve) -> bool {
    if !(2..=MAX_COLOR_CURVE_POINTS).contains(&curve.points.len()) {
        return false;
    }
    if curve.points.first().map(|point| point.x) != Some(0.0)
        || curve.points.last().map(|point| point.x) != Some(1.0)
    {
        return false;
    }
    curve.points.windows(2).all(|points| {
        let left = points[0];
        let right = points[1];
        left.x.is_finite()
            && left.y.is_finite()
            && right.x.is_finite()
            && right.y.is_finite()
            && (0.0..=1.0).contains(&left.x)
            && (0.0..=1.0).contains(&left.y)
            && (0.0..=1.0).contains(&right.x)
            && (0.0..=1.0).contains(&right.y)
            && ((left.x * 255.0) as i32) < ((right.x * 255.0) as i32)
    })
}

fn normalize_rgb_curves(curves: &RgbCurves) -> bool {
    normalize_color_curve(&curves.master)
        && normalize_color_curve(&curves.red)
        && normalize_color_curve(&curves.green)
        && normalize_color_curve(&curves.blue)
}

#[cfg(test)]
fn identity_curve_lut() -> [[f32; 4]; COLOR_CURVE_LUT_SAMPLES] {
    std::array::from_fn(|index| {
        let value = index as f32 / (COLOR_CURVE_LUT_SAMPLES - 1) as f32;
        [value, value, value, 0.0]
    })
}

#[cfg(test)]
fn compile_rgb_curve_lut(curves: &RgbCurves) -> [[f32; 4]; COLOR_CURVE_LUT_SAMPLES] {
    std::array::from_fn(|index| {
        let input = index as f32 / (COLOR_CURVE_LUT_SAMPLES - 1) as f32;
        [
            natural_curve_sample(&curves.master, natural_curve_sample(&curves.red, input)),
            natural_curve_sample(&curves.master, natural_curve_sample(&curves.green, input)),
            natural_curve_sample(&curves.master, natural_curve_sample(&curves.blue, input)),
            0.0,
        ]
    })
}

/// Natural cubic spline interpolation, matching FFmpeg's `curves=interp=natural` mode.
fn natural_curve_sample(curve: &ColorCurve, input: f32) -> f32 {
    let points = &curve.points;
    let count = points.len();
    debug_assert!((2..=MAX_COLOR_CURVE_POINTS).contains(&count));
    let mut h = [0.0; MAX_COLOR_CURVE_POINTS - 1];
    let mut alpha = [0.0; MAX_COLOR_CURVE_POINTS];
    for index in 0..count - 1 {
        h[index] = points[index + 1].x - points[index].x;
    }
    for index in 1..count - 1 {
        alpha[index] = 3.0 / h[index] * (points[index + 1].y - points[index].y)
            - 3.0 / h[index - 1] * (points[index].y - points[index - 1].y);
    }
    let mut lower = [0.0; MAX_COLOR_CURVE_POINTS];
    let mut mu = [0.0; MAX_COLOR_CURVE_POINTS];
    let mut z = [0.0; MAX_COLOR_CURVE_POINTS];
    lower[0] = 1.0;
    for index in 1..count - 1 {
        lower[index] =
            2.0 * (points[index + 1].x - points[index - 1].x) - h[index - 1] * mu[index - 1];
        mu[index] = h[index] / lower[index];
        z[index] = (alpha[index] - h[index - 1] * z[index - 1]) / lower[index];
    }
    let mut c = [0.0; MAX_COLOR_CURVE_POINTS];
    let mut b = [0.0; MAX_COLOR_CURVE_POINTS - 1];
    let mut d = [0.0; MAX_COLOR_CURVE_POINTS - 1];
    for index in (0..count - 1).rev() {
        c[index] = z[index] - mu[index] * c[index + 1];
        b[index] = (points[index + 1].y - points[index].y) / h[index]
            - h[index] * (c[index + 1] + 2.0 * c[index]) / 3.0;
        d[index] = (c[index + 1] - c[index]) / (3.0 * h[index]);
    }
    let index = points
        .partition_point(|point| point.x <= input.clamp(0.0, 1.0))
        .saturating_sub(1)
        .min(count - 2);
    let distance = input.clamp(0.0, 1.0) - points[index].x;
    (points[index].y
        + b[index] * distance
        + c[index] * distance * distance
        + d[index] * distance.powi(3))
    .clamp(0.0, 1.0)
}

fn normalize_video_effects(effects: &mut [VideoEffectNode]) -> bool {
    if effects.len() > MAX_VIDEO_EFFECTS_PER_CLIP {
        return false;
    }
    let mut ids = std::collections::HashSet::new();
    effects.iter_mut().all(|node| {
        node.id.0 != 0
            && ids.insert(node.id)
            && match &mut node.kind {
                VideoEffectKind::BrightnessContrast(effect) => {
                    normalize_animated_scalar(
                        &mut effect.brightness,
                        MIN_BRIGHTNESS,
                        MAX_BRIGHTNESS,
                    ) && normalize_animated_scalar(&mut effect.contrast, MIN_CONTRAST, MAX_CONTRAST)
                        && normalize_animated_scalar(
                            &mut effect.temperature,
                            MIN_TEMPERATURE,
                            MAX_TEMPERATURE,
                        )
                        && normalize_animated_scalar(&mut effect.tint, MIN_TINT, MAX_TINT)
                        && normalize_animated_scalar(
                            &mut effect.saturation,
                            MIN_SATURATION,
                            MAX_SATURATION,
                        )
                        && normalize_animated_scalar(
                            &mut effect.exposure,
                            MIN_EXPOSURE,
                            MAX_EXPOSURE,
                        )
                        && normalize_animated_scalar(
                            &mut effect.highlights,
                            MIN_HIGHLIGHTS,
                            MAX_HIGHLIGHTS,
                        )
                        && normalize_animated_scalar(&mut effect.shadows, MIN_SHADOWS, MAX_SHADOWS)
                        && normalize_animated_scalar(&mut effect.whites, MIN_WHITES, MAX_WHITES)
                        && normalize_animated_scalar(&mut effect.blacks, MIN_BLACKS, MAX_BLACKS)
                        && normalize_rgb_curves(&effect.curves)
                }
                VideoEffectKind::Vignette(effect) => {
                    normalize_animated_scalar(
                        &mut effect.amount,
                        MIN_VIGNETTE_AMOUNT,
                        MAX_VIGNETTE_AMOUNT,
                    ) && normalize_animated_scalar(
                        &mut effect.midpoint,
                        MIN_VIGNETTE_MIDPOINT,
                        MAX_VIGNETTE_MIDPOINT,
                    ) && normalize_animated_scalar(
                        &mut effect.feather,
                        MIN_VIGNETTE_FEATHER,
                        MAX_VIGNETTE_FEATHER,
                    ) && normalize_animated_scalar(
                        &mut effect.center_x,
                        MIN_VIGNETTE_CENTER,
                        MAX_VIGNETTE_CENTER,
                    ) && normalize_animated_scalar(
                        &mut effect.center_y,
                        MIN_VIGNETTE_CENTER,
                        MAX_VIGNETTE_CENTER,
                    )
                }
            }
    })
}

fn color_scalar(
    clip: &Clip,
    effect_id: VideoEffectId,
    parameter: ColorParameter,
) -> Option<&AnimatedScalar> {
    let node = clip
        .video_effects
        .iter()
        .find(|node| node.id == effect_id)?;
    match (&node.kind, parameter) {
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Brightness) => {
            Some(&effect.brightness)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Contrast) => {
            Some(&effect.contrast)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Temperature) => {
            Some(&effect.temperature)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Tint) => Some(&effect.tint),
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Saturation) => {
            Some(&effect.saturation)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Exposure) => {
            Some(&effect.exposure)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Highlights) => {
            Some(&effect.highlights)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Shadows) => {
            Some(&effect.shadows)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Whites) => {
            Some(&effect.whites)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Blacks) => {
            Some(&effect.blacks)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteAmount) => Some(&effect.amount),
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteMidpoint) => {
            Some(&effect.midpoint)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteFeather) => {
            Some(&effect.feather)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteCenterX) => {
            Some(&effect.center_x)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteCenterY) => {
            Some(&effect.center_y)
        }
        _ => None,
    }
}

fn color_scalar_mut(
    clip: &mut Clip,
    effect_id: VideoEffectId,
    parameter: ColorParameter,
) -> Option<&mut AnimatedScalar> {
    let node = clip
        .video_effects
        .iter_mut()
        .find(|node| node.id == effect_id)?;
    match (&mut node.kind, parameter) {
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Brightness) => {
            Some(&mut effect.brightness)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Contrast) => {
            Some(&mut effect.contrast)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Temperature) => {
            Some(&mut effect.temperature)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Tint) => {
            Some(&mut effect.tint)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Saturation) => {
            Some(&mut effect.saturation)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Exposure) => {
            Some(&mut effect.exposure)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Highlights) => {
            Some(&mut effect.highlights)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Shadows) => {
            Some(&mut effect.shadows)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Whites) => {
            Some(&mut effect.whites)
        }
        (VideoEffectKind::BrightnessContrast(effect), ColorParameter::Blacks) => {
            Some(&mut effect.blacks)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteAmount) => {
            Some(&mut effect.amount)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteMidpoint) => {
            Some(&mut effect.midpoint)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteFeather) => {
            Some(&mut effect.feather)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteCenterX) => {
            Some(&mut effect.center_x)
        }
        (VideoEffectKind::Vignette(effect), ColorParameter::VignetteCenterY) => {
            Some(&mut effect.center_y)
        }
        _ => None,
    }
}

fn color_parameter_value(parameter: ColorParameter, value: f32) -> f32 {
    match parameter {
        ColorParameter::Brightness => value.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS),
        ColorParameter::Contrast => value.clamp(MIN_CONTRAST, MAX_CONTRAST),
        ColorParameter::Temperature => value.clamp(MIN_TEMPERATURE, MAX_TEMPERATURE),
        ColorParameter::Tint => value.clamp(MIN_TINT, MAX_TINT),
        ColorParameter::Saturation => value.clamp(MIN_SATURATION, MAX_SATURATION),
        ColorParameter::Exposure => value.clamp(MIN_EXPOSURE, MAX_EXPOSURE),
        ColorParameter::Highlights => value.clamp(MIN_HIGHLIGHTS, MAX_HIGHLIGHTS),
        ColorParameter::Shadows => value.clamp(MIN_SHADOWS, MAX_SHADOWS),
        ColorParameter::Whites => value.clamp(MIN_WHITES, MAX_WHITES),
        ColorParameter::Blacks => value.clamp(MIN_BLACKS, MAX_BLACKS),
        ColorParameter::VignetteAmount => value.clamp(MIN_VIGNETTE_AMOUNT, MAX_VIGNETTE_AMOUNT),
        ColorParameter::VignetteMidpoint => {
            value.clamp(MIN_VIGNETTE_MIDPOINT, MAX_VIGNETTE_MIDPOINT)
        }
        ColorParameter::VignetteFeather => value.clamp(MIN_VIGNETTE_FEATHER, MAX_VIGNETTE_FEATHER),
        ColorParameter::VignetteCenterX | ColorParameter::VignetteCenterY => {
            value.clamp(MIN_VIGNETTE_CENTER, MAX_VIGNETTE_CENTER)
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub gain_db: f32,
    /// Normalized stereo balance, from fully left (-1) to fully right (1).
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub effects: Vec<AudioEffect>,
    /// Always sorted by start and non-overlapping.
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn audio_is_audible(&self, any_solo: bool) -> bool {
        matches!(self.kind, TrackKind::Audio) && !self.muted && (!any_solo || self.solo)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkedClipPair {
    pub link_id: LinkId,
    pub video: ClipId,
    pub audio: ClipId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RazorSplit {
    pub left: ClipId,
    pub right: ClipId,
}

/// Serializable timeline state. Restore through [`Timeline::from_snapshot`]
/// so untrusted or stale data is validated before use.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineSnapshot {
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub titles: Vec<TitleOverlay>,
    #[serde(default)]
    pub transitions: Vec<VideoTransition>,
    #[serde(default)]
    pub audio_transitions: Vec<AudioTransition>,
}

/// One reversible timeline delta. History stores these compact inverse operations rather than
/// retaining whole project or timeline snapshots.
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineEdit {
    InsertTitle {
        title: TitleOverlay,
        index: usize,
    },
    RemoveTitle {
        title: TitleOverlay,
        index: usize,
    },
    PatchTitle {
        before: TitleOverlay,
        after: TitleOverlay,
    },
    InsertClip {
        clip: Clip,
        track: TrackId,
        index: usize,
    },
    RemoveClip {
        clip: Clip,
        track: TrackId,
        index: usize,
    },
    PatchClip {
        before: Clip,
        after: Clip,
    },
    InsertTrack {
        track: Track,
        index: usize,
    },
    RemoveTrack {
        track: Track,
        index: usize,
    },
    PatchTrack {
        before: Track,
        after: Track,
    },
    InsertTransition {
        transition: VideoTransition,
        index: usize,
    },
    RemoveTransition {
        transition: VideoTransition,
        index: usize,
    },
    PatchTransition {
        before: VideoTransition,
        after: VideoTransition,
    },
    InsertAudioTransition {
        transition: AudioTransition,
        index: usize,
    },
    RemoveAudioTransition {
        transition: AudioTransition,
        index: usize,
    },
    PatchAudioTransition {
        before: AudioTransition,
        after: AudioTransition,
    },
}

#[derive(Clone, Debug)]
struct TimelineEditBatch {
    edits: Vec<TimelineEdit>,
    structural: bool,
    track_cache_structural: bool,
}

/// Capped undo/redo history. Snapshots may be supplied briefly to calculate a delta, but only
/// inverse operations are retained. This keeps the long-lived history proportional to edits.
#[derive(Clone, Debug)]
pub struct UndoStack {
    undo: Vec<TimelineEditBatch>,
    redo: Vec<TimelineEditBatch>,
    capacity: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}

impl UndoStack {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn len(&self) -> usize {
        self.undo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.undo.is_empty()
    }

    /// Records the changed clips/tracks between two validated states. Returns false for a no-op.
    pub fn record(&mut self, before: &TimelineSnapshot, after: &TimelineSnapshot) -> bool {
        let Some(batch) = timeline_delta(before, after) else {
            return false;
        };
        self.push_batch(batch);
        true
    }

    /// Records the delta against the current live timeline without cloning a second complete
    /// snapshot. Pointer gestures already retain their before-state; avoiding an after-state copy
    /// keeps large-timeline release events inside the interaction budget.
    pub fn record_current(&mut self, before: &TimelineSnapshot, after: &Timeline) -> bool {
        let Some(batch) = timeline_state_delta(
            &before.tracks,
            &before.titles,
            &before.transitions,
            &before.audio_transitions,
            &after.tracks,
            &after.titles,
            &after.transitions,
            &after.audio_transitions,
        ) else {
            return false;
        };
        self.push_batch(batch);
        true
    }

    fn push_batch(&mut self, batch: TimelineEditBatch) {
        if self.undo.len() == self.capacity {
            self.undo.remove(0);
        }
        self.undo.push(batch);
        self.redo.clear();
    }

    pub fn undo(&mut self, timeline: &mut Timeline) -> bool {
        let Some(batch) = self.undo.pop() else {
            return false;
        };
        timeline.apply_history_batch(&batch, false);
        self.redo.push(batch);
        true
    }

    pub fn redo(&mut self, timeline: &mut Timeline) -> bool {
        let Some(batch) = self.redo.pop() else {
            return false;
        };
        timeline.apply_history_batch(&batch, true);
        self.undo.push(batch);
        true
    }
}

fn timeline_delta(
    before: &TimelineSnapshot,
    after: &TimelineSnapshot,
) -> Option<TimelineEditBatch> {
    timeline_state_delta(
        &before.tracks,
        &before.titles,
        &before.transitions,
        &before.audio_transitions,
        &after.tracks,
        &after.titles,
        &after.transitions,
        &after.audio_transitions,
    )
}

// Keeping before/after slices explicit prevents hidden full-snapshot ownership in undo capture.
#[allow(clippy::too_many_arguments)]
fn timeline_state_delta(
    before_tracks: &[Track],
    before_titles: &[TitleOverlay],
    before_transitions: &[VideoTransition],
    before_audio_transitions: &[AudioTransition],
    after_tracks: &[Track],
    after_titles: &[TitleOverlay],
    after_transitions: &[VideoTransition],
    after_audio_transitions: &[AudioTransition],
) -> Option<TimelineEditBatch> {
    let mut edits = timeline_tracks_delta(before_tracks, after_tracks)
        .map(|batch| batch.edits)
        .unwrap_or_default();
    let before_by_id = before_titles
        .iter()
        .enumerate()
        .map(|(i, title)| (title.id, (i, title)))
        .collect::<HashMap<_, _>>();
    let after_by_id = after_titles
        .iter()
        .enumerate()
        .map(|(i, title)| (title.id, (i, title)))
        .collect::<HashMap<_, _>>();
    let mut removed = before_by_id
        .iter()
        .filter(|(id, _)| !after_by_id.contains_key(id))
        .map(|(_, (i, t))| (*i, (*t).clone()))
        .collect::<Vec<_>>();
    removed.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    edits.extend(
        removed
            .into_iter()
            .map(|(index, title)| TimelineEdit::RemoveTitle { title, index }),
    );
    for title in before_titles {
        if let Some((_, after)) = after_by_id.get(&title.id)
            && title != *after
        {
            edits.push(TimelineEdit::PatchTitle {
                before: title.clone(),
                after: (*after).clone(),
            });
        }
    }
    let mut inserted = after_by_id
        .iter()
        .filter(|(id, _)| !before_by_id.contains_key(id))
        .map(|(_, (i, t))| (*i, (*t).clone()))
        .collect::<Vec<_>>();
    inserted.sort_by_key(|(i, _)| *i);
    edits.extend(
        inserted
            .into_iter()
            .map(|(index, title)| TimelineEdit::InsertTitle { title, index }),
    );
    append_transition_delta(before_transitions, after_transitions, &mut edits);
    append_audio_transition_delta(
        before_audio_transitions,
        after_audio_transitions,
        &mut edits,
    );
    (!edits.is_empty()).then(|| timeline_edit_batch(edits))
}

fn append_audio_transition_delta(
    before: &[AudioTransition],
    after: &[AudioTransition],
    edits: &mut Vec<TimelineEdit>,
) {
    let before_by_id = before
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id, (i, item)))
        .collect::<HashMap<_, _>>();
    let after_by_id = after
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id, (i, item)))
        .collect::<HashMap<_, _>>();
    let mut removed = before_by_id
        .iter()
        .filter(|(id, _)| !after_by_id.contains_key(id))
        .map(|(_, (i, item))| (*i, (*item).clone()))
        .collect::<Vec<_>>();
    removed.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    edits.extend(
        removed
            .into_iter()
            .map(|(index, transition)| TimelineEdit::RemoveAudioTransition { transition, index }),
    );
    for item in before {
        if let Some((_, after_item)) = after_by_id.get(&item.id)
            && item != *after_item
        {
            edits.push(TimelineEdit::PatchAudioTransition {
                before: item.clone(),
                after: (*after_item).clone(),
            });
        }
    }
    let mut inserted = after_by_id
        .iter()
        .filter(|(id, _)| !before_by_id.contains_key(id))
        .map(|(_, (i, item))| (*i, (*item).clone()))
        .collect::<Vec<_>>();
    inserted.sort_by_key(|(i, _)| *i);
    edits.extend(
        inserted
            .into_iter()
            .map(|(index, transition)| TimelineEdit::InsertAudioTransition { transition, index }),
    );
}

fn append_transition_delta(
    before: &[VideoTransition],
    after: &[VideoTransition],
    edits: &mut Vec<TimelineEdit>,
) {
    let before_by_id = before
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id, (i, item)))
        .collect::<HashMap<_, _>>();
    let after_by_id = after
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id, (i, item)))
        .collect::<HashMap<_, _>>();
    let mut removed = before_by_id
        .iter()
        .filter(|(id, _)| !after_by_id.contains_key(id))
        .map(|(_, (i, item))| (*i, (*item).clone()))
        .collect::<Vec<_>>();
    removed.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
    edits.extend(
        removed
            .into_iter()
            .map(|(index, transition)| TimelineEdit::RemoveTransition { transition, index }),
    );
    for item in before {
        if let Some((_, after_item)) = after_by_id.get(&item.id)
            && item != *after_item
        {
            edits.push(TimelineEdit::PatchTransition {
                before: item.clone(),
                after: (*after_item).clone(),
            });
        }
    }
    let mut inserted = after_by_id
        .iter()
        .filter(|(id, _)| !before_by_id.contains_key(id))
        .map(|(_, (i, item))| (*i, (*item).clone()))
        .collect::<Vec<_>>();
    inserted.sort_by_key(|(i, _)| *i);
    edits.extend(
        inserted
            .into_iter()
            .map(|(index, transition)| TimelineEdit::InsertTransition { transition, index }),
    );
}

fn timeline_tracks_delta(before: &[Track], after: &[Track]) -> Option<TimelineEditBatch> {
    if before == after {
        return None;
    }
    if let Some(edits) = same_cardinality_timeline_delta(before, after) {
        return Some(timeline_edit_batch(edits));
    }
    let before_tracks = before
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id, (index, track)))
        .collect::<HashMap<_, _>>();
    let after_tracks = after
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id, (index, track)))
        .collect::<HashMap<_, _>>();
    let mut edits = Vec::new();

    let mut removed_tracks = before_tracks
        .iter()
        .filter(|(id, _)| !after_tracks.contains_key(id))
        .map(|(_, (index, track))| (*index, (*track).clone()))
        .collect::<Vec<_>>();
    removed_tracks.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    edits.extend(
        removed_tracks
            .into_iter()
            .map(|(index, track)| TimelineEdit::RemoveTrack { track, index }),
    );

    let mut inserted_tracks = after_tracks
        .iter()
        .filter(|(id, _)| !before_tracks.contains_key(id))
        .map(|(_, (index, track))| (*index, (*track).clone()))
        .collect::<Vec<_>>();
    inserted_tracks.sort_by_key(|(index, _)| *index);
    edits.extend(
        inserted_tracks
            .into_iter()
            .map(|(index, track)| TimelineEdit::InsertTrack { track, index }),
    );

    for before_track in before {
        let Some((_, after_track)) = after_tracks.get(&before_track.id) else {
            continue;
        };
        if !same_track_controls(before_track, after_track) {
            edits.push(TimelineEdit::PatchTrack {
                before: track_controls_snapshot(before_track),
                after: track_controls_snapshot(after_track),
            });
        }
        let before_clips = before_track
            .clips
            .iter()
            .enumerate()
            .map(|(index, clip)| (clip.id, (index, clip)))
            .collect::<HashMap<_, _>>();
        let after_clips = after_track
            .clips
            .iter()
            .enumerate()
            .map(|(index, clip)| (clip.id, (index, clip)))
            .collect::<HashMap<_, _>>();
        let mut removed = before_clips
            .iter()
            .filter(|(id, _)| !after_clips.contains_key(id))
            .map(|(_, (index, clip))| (*index, (*clip).clone()))
            .collect::<Vec<_>>();
        removed.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
        edits.extend(
            removed
                .into_iter()
                .map(|(index, clip)| TimelineEdit::RemoveClip {
                    clip,
                    track: before_track.id,
                    index,
                }),
        );
        for before_clip in &before_track.clips {
            if let Some((_, after_clip)) = after_clips.get(&before_clip.id)
                && before_clip != *after_clip
            {
                edits.push(TimelineEdit::PatchClip {
                    before: before_clip.clone(),
                    after: (*after_clip).clone(),
                });
            }
        }
        let mut inserted = after_clips
            .iter()
            .filter(|(id, _)| !before_clips.contains_key(id))
            .map(|(_, (index, clip))| (*index, (*clip).clone()))
            .collect::<Vec<_>>();
        inserted.sort_by_key(|(index, _)| *index);
        edits.extend(
            inserted
                .into_iter()
                .map(|(index, clip)| TimelineEdit::InsertClip {
                    clip,
                    track: after_track.id,
                    index,
                }),
        );
    }

    Some(timeline_edit_batch(edits))
}

/// Fast path for ordinary interaction edits. Track membership and clip cardinality stay stable
/// for move, trim, roll, slip, gain, fade, replace, and mute. Comparing the sorted slices avoids
/// constructing two large hash maps after every pointer gesture. A single moved clip may change
/// its sorted position; that case is a rotation of one otherwise-identical contiguous range.
fn same_cardinality_timeline_delta(before: &[Track], after: &[Track]) -> Option<Vec<TimelineEdit>> {
    if before.len() != after.len() {
        return None;
    }
    let mut edits = Vec::new();
    for (before_track, after_track) in before.iter().zip(after) {
        if before_track.id != after_track.id
            || before_track.kind != after_track.kind
            || before_track.clips.len() != after_track.clips.len()
        {
            return None;
        }
        if !same_track_controls(before_track, after_track) {
            edits.push(TimelineEdit::PatchTrack {
                before: track_controls_snapshot(before_track),
                after: track_controls_snapshot(after_track),
            });
        }
        append_same_cardinality_clip_delta(&before_track.clips, &after_track.clips, &mut edits)?;
    }
    Some(edits)
}

fn same_track_controls(left: &Track, right: &Track) -> bool {
    left.id == right.id
        && left.kind == right.kind
        && left.muted == right.muted
        && left.solo == right.solo
        && left.gain_db == right.gain_db
        && left.pan == right.pan
        && left.effects == right.effects
}

fn track_controls_snapshot(track: &Track) -> Track {
    Track {
        id: track.id,
        kind: track.kind,
        muted: track.muted,
        solo: track.solo,
        gain_db: track.gain_db,
        pan: track.pan,
        effects: track.effects.clone(),
        clips: Vec::new(),
    }
}

fn append_same_cardinality_clip_delta(
    before: &[Clip],
    after: &[Clip],
    edits: &mut Vec<TimelineEdit>,
) -> Option<()> {
    if before
        .iter()
        .zip(after)
        .all(|(left, right)| left.id == right.id)
    {
        edits.extend(
            before
                .iter()
                .zip(after)
                .filter(|(left, right)| left != right)
                .map(|(before, after)| TimelineEdit::PatchClip {
                    before: before.clone(),
                    after: after.clone(),
                }),
        );
        return Some(());
    }

    let prefix = before
        .iter()
        .zip(after)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let end = before.len().saturating_sub(suffix);
    let before_middle = &before[prefix..end];
    let after_middle = &after[prefix..end];
    let last = before_middle.len().checked_sub(1)?;

    let moved = if after_middle[0].id == before_middle[last].id
        && before_middle[..last] == after_middle[1..]
    {
        Some((&before_middle[last], &after_middle[0]))
    } else if before_middle[0].id == after_middle[last].id
        && before_middle[1..] == after_middle[..last]
    {
        Some((&before_middle[0], &after_middle[last]))
    } else {
        None
    }?;
    edits.push(TimelineEdit::PatchClip {
        before: moved.0.clone(),
        after: moved.1.clone(),
    });
    Some(())
}

fn timeline_edit_batch(edits: Vec<TimelineEdit>) -> TimelineEditBatch {
    let structural = edits.iter().any(|edit| match edit {
        TimelineEdit::PatchTrack { .. }
        | TimelineEdit::PatchTitle { .. }
        | TimelineEdit::InsertTransition { .. }
        | TimelineEdit::RemoveTransition { .. }
        | TimelineEdit::PatchTransition { .. }
        | TimelineEdit::InsertAudioTransition { .. }
        | TimelineEdit::RemoveAudioTransition { .. }
        | TimelineEdit::PatchAudioTransition { .. } => false,
        TimelineEdit::PatchClip { before, after } => {
            before.track_id != after.track_id
                || before.start != after.start
                || before.duration != after.duration
        }
        TimelineEdit::InsertClip { .. }
        | TimelineEdit::RemoveClip { .. }
        | TimelineEdit::InsertTrack { .. }
        | TimelineEdit::RemoveTrack { .. } => true,
        TimelineEdit::InsertTitle { .. } | TimelineEdit::RemoveTitle { .. } => true,
    });
    let track_cache_structural = edits.iter().any(|edit| match edit {
        TimelineEdit::PatchClip { before, after } => {
            before.track_id != after.track_id
                || before.start != after.start
                || before.duration != after.duration
        }
        TimelineEdit::InsertClip { .. }
        | TimelineEdit::RemoveClip { .. }
        | TimelineEdit::InsertTrack { .. }
        | TimelineEdit::RemoveTrack { .. } => true,
        TimelineEdit::PatchTrack { .. }
        | TimelineEdit::InsertTitle { .. }
        | TimelineEdit::RemoveTitle { .. }
        | TimelineEdit::PatchTitle { .. }
        | TimelineEdit::InsertTransition { .. }
        | TimelineEdit::RemoveTransition { .. }
        | TimelineEdit::PatchTransition { .. }
        | TimelineEdit::InsertAudioTransition { .. }
        | TimelineEdit::RemoveAudioTransition { .. }
        | TimelineEdit::PatchAudioTransition { .. } => false,
    });
    TimelineEditBatch {
        edits,
        structural,
        track_cache_structural,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimelineSnapshotError {
    InvalidTrackId(TrackId),
    DuplicateTrackId(TrackId),
    InvalidClipId(ClipId),
    DuplicateClipId(ClipId),
    InvalidTransitionId(TransitionId),
    DuplicateTransitionId(TransitionId),
    InvalidTransition { transition: TransitionId },
    InvalidAudioTransitionId(AudioTransitionId),
    DuplicateAudioTransitionId(AudioTransitionId),
    InvalidAudioTransition { transition: AudioTransitionId },
    InvalidLinkId(LinkId),
    TrackMismatch { clip: ClipId, track: TrackId },
    NegativeStart { clip: ClipId },
    InvalidDuration { clip: ClipId },
    NegativeSourceIn { clip: ClipId },
    TickOverflow { clip: ClipId },
    UnsortedOrOverlapping { track: TrackId, clip: ClipId },
    NonFiniteGain { clip: ClipId },
    NonFiniteChannelGain { clip: ClipId },
    NonFiniteTrackGain { track: TrackId },
    NonFiniteTrackPan { track: TrackId },
    InvalidClipEffect { clip: ClipId },
    InvalidVideoEffect { clip: ClipId },
    InvalidTrackEffect { track: TrackId },
    NonFiniteFadeCurve { clip: ClipId },
    NonFiniteTransform { clip: ClipId },
    InvalidTitleId(TitleId),
    DuplicateTitleId(TitleId),
    InvalidTitle { title: TitleId },
    TitleTickOverflow { title: TitleId },
    IdExhausted,
}

impl fmt::Display for TimelineSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid timeline snapshot: {self:?}")
    }
}

impl std::error::Error for TimelineSnapshotError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimelineError {
    UnknownTrack(TrackId),
    UnknownClip(ClipId),
    UnknownTransition(TransitionId),
    UnknownAudioTransition(AudioTransitionId),
    UnknownTitle(TitleId),
    TitleIdMismatch { expected: TitleId, actual: TitleId },
    InvalidTitle,
    IdExhausted,
    NoTrackOfKind(TrackKind),
    InvalidDuration,
    InvalidMediaDuration,
    SourceOutsideMedia { clip: ClipId, media: MediaId },
    NegativeStart { clip: ClipId },
    NegativeSourceIn { clip: ClipId },
    RollNotAdjacent { left: ClipId, right: ClipId },
    TickOverflow,
    Overlap { track: TrackId, clip: ClipId },
    AudioOnly(ClipId),
    VideoOnly(ClipId),
    AudioTrackOnly(TrackId),
    NonFiniteAudioControl,
    NonFiniteTransform,
    InvalidAudioEffect,
    InvalidVideoEffect,
    InvalidTransition,
    InvariantViolation,
}

impl fmt::Display for TimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "timeline edit failed: {self:?}")
    }
}

impl std::error::Error for TimelineError {}

/// Audio gain range intentionally has a finite, UI-friendly floor.
pub const MIN_GAIN_DB: f32 = -96.0;
pub const MAX_GAIN_DB: f32 = 24.0;
pub const MIN_PAN: f32 = -1.0;
pub const MAX_PAN: f32 = 1.0;
pub const MIN_FADE_CURVE: f32 = -1.0;
pub const MAX_FADE_CURVE: f32 = 1.0;

#[derive(Clone, Debug)]
pub struct Timeline {
    pub tracks: Vec<Track>,
    titles: Vec<TitleOverlay>,
    transitions: Vec<VideoTransition>,
    audio_transitions: Vec<AudioTransition>,
    /// Internal hot-edit lookup. Rebuilt after structural edits; timing-only
    /// edits retain vector positions and update clips in place.
    clip_locations: HashMap<ClipId, ClipLocation>,
    next_track_id: u32,
    next_clip_id: u32,
    next_link_id: u32,
    next_title_id: u32,
    next_transition_id: u32,
    next_audio_transition_id: u32,
    generation: u64,
    structural_generation: u64,
}

#[derive(Clone, Copy, Debug)]
struct ClipLocation {
    track_index: usize,
    clip_index: usize,
}

#[derive(Clone, Copy, Debug)]
struct TimingChange {
    id: ClipId,
    start: Tick,
    duration: Tick,
    source_in: Tick,
}

impl Timeline {
    /// Starts with the common editorial layout: V1–V3 and A1–A3.
    pub fn new_default() -> Self {
        let mut tracks = Vec::with_capacity(6);
        let mut next_track_id = 1;
        for kind in [
            TrackKind::Video,
            TrackKind::Video,
            TrackKind::Video,
            TrackKind::Audio,
            TrackKind::Audio,
            TrackKind::Audio,
        ] {
            tracks.push(Track {
                id: TrackId(next_track_id),
                kind,
                muted: false,
                solo: false,
                gain_db: 0.0,
                pan: 0.0,
                effects: Vec::new(),
                clips: Vec::new(),
            });
            next_track_id += 1;
        }
        Self {
            tracks,
            titles: Vec::new(),
            transitions: Vec::new(),
            audio_transitions: Vec::new(),
            clip_locations: HashMap::new(),
            next_track_id,
            next_clip_id: 1,
            next_link_id: 1,
            next_title_id: 1,
            next_transition_id: 1,
            next_audio_transition_id: 1,
            generation: 1,
            structural_generation: 1,
        }
    }

    pub fn snapshot(&self) -> TimelineSnapshot {
        TimelineSnapshot {
            tracks: self.tracks.clone(),
            titles: self.titles.clone(),
            transitions: self.transitions.clone(),
            audio_transitions: self.audio_transitions.clone(),
        }
    }

    /// Restores a validated snapshot, normalizing finite gain and fade values
    /// with the same bounds used by the public editing APIs.
    pub fn from_snapshot(mut snapshot: TimelineSnapshot) -> Result<Self, TimelineSnapshotError> {
        use std::collections::HashSet;

        let mut track_ids = HashSet::new();
        let mut clip_ids = HashSet::new();
        let mut max_track_id = 0;
        let mut max_clip_id = 0;
        let mut max_link_id = 0;
        let mut title_ids = HashSet::new();
        let mut max_title_id = 0;
        let mut transition_ids = HashSet::new();
        let mut cuts = HashSet::new();
        let mut max_transition_id = 0;
        let mut audio_transition_ids = HashSet::new();
        let mut audio_cuts = HashSet::new();
        let mut max_audio_transition_id = 0;

        for track in &mut snapshot.tracks {
            if track.id.0 == 0 {
                return Err(TimelineSnapshotError::InvalidTrackId(track.id));
            }
            if !track_ids.insert(track.id) {
                return Err(TimelineSnapshotError::DuplicateTrackId(track.id));
            }
            max_track_id = max_track_id.max(track.id.0);
            if !track.gain_db.is_finite() {
                return Err(TimelineSnapshotError::NonFiniteTrackGain { track: track.id });
            }
            if !track.pan.is_finite() {
                return Err(TimelineSnapshotError::NonFiniteTrackPan { track: track.id });
            }
            if track.effects.len() > MAX_AUDIO_EFFECTS_PER_SCOPE
                || track.effects.iter().any(|effect| !effect.is_valid())
            {
                return Err(TimelineSnapshotError::InvalidTrackEffect { track: track.id });
            }
            track.gain_db = track.gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
            track.pan = track.pan.clamp(MIN_PAN, MAX_PAN);

            let mut previous_end = None;
            for clip in &mut track.clips {
                if clip.id.0 == 0 {
                    return Err(TimelineSnapshotError::InvalidClipId(clip.id));
                }
                if !clip_ids.insert(clip.id) {
                    return Err(TimelineSnapshotError::DuplicateClipId(clip.id));
                }
                if clip.track_id != track.id {
                    return Err(TimelineSnapshotError::TrackMismatch {
                        clip: clip.id,
                        track: track.id,
                    });
                }
                if clip.start.0 < 0 {
                    return Err(TimelineSnapshotError::NegativeStart { clip: clip.id });
                }
                if clip.duration.0 <= 0 {
                    return Err(TimelineSnapshotError::InvalidDuration { clip: clip.id });
                }
                if clip.source_in.0 < 0 {
                    return Err(TimelineSnapshotError::NegativeSourceIn { clip: clip.id });
                }
                let end = clip
                    .start
                    .0
                    .checked_add(clip.duration.0)
                    .ok_or(TimelineSnapshotError::TickOverflow { clip: clip.id })?;
                if previous_end.is_some_and(|previous_end| previous_end > clip.start.0) {
                    return Err(TimelineSnapshotError::UnsortedOrOverlapping {
                        track: track.id,
                        clip: clip.id,
                    });
                }
                previous_end = Some(end);
                if !clip.gain_db.is_finite() {
                    return Err(TimelineSnapshotError::NonFiniteGain { clip: clip.id });
                }
                if !clip.gain_left_db.is_finite() || !clip.gain_right_db.is_finite() {
                    return Err(TimelineSnapshotError::NonFiniteChannelGain { clip: clip.id });
                }
                if clip.effects.len() > MAX_AUDIO_EFFECTS_PER_SCOPE
                    || clip.effects.iter().any(|effect| !effect.is_valid())
                {
                    return Err(TimelineSnapshotError::InvalidClipEffect { clip: clip.id });
                }
                if !normalize_video_effects(&mut clip.video_effects) {
                    return Err(TimelineSnapshotError::InvalidVideoEffect { clip: clip.id });
                }
                if !clip.fade_in.curve.is_finite() || !clip.fade_out.curve.is_finite() {
                    return Err(TimelineSnapshotError::NonFiniteFadeCurve { clip: clip.id });
                }
                if !clip.transform.is_finite() {
                    return Err(TimelineSnapshotError::NonFiniteTransform { clip: clip.id });
                }
                clip.gain_db = clip.gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
                clip.gain_left_db = clip.gain_left_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
                clip.gain_right_db = clip.gain_right_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
                clip.fade_in.curve = clip.fade_in.curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE);
                clip.fade_out.curve = clip.fade_out.curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE);
                clip.transform = clip.transform.clamped();
                Self::clamp_fades_to_clip(clip);
                max_clip_id = max_clip_id.max(clip.id.0);
                if let Some(link_id) = clip.link_id {
                    if link_id.0 == 0 {
                        return Err(TimelineSnapshotError::InvalidLinkId(link_id));
                    }
                    max_link_id = max_link_id.max(link_id.0);
                }
            }
        }

        for title in &mut snapshot.titles {
            if title.id.0 == 0 {
                return Err(TimelineSnapshotError::InvalidTitleId(title.id));
            }
            if !title_ids.insert(title.id) {
                return Err(TimelineSnapshotError::DuplicateTitleId(title.id));
            }
            if !Self::normalize_title(title) {
                return Err(TimelineSnapshotError::InvalidTitle { title: title.id });
            }
            title
                .start
                .0
                .checked_add(title.duration.0)
                .ok_or(TimelineSnapshotError::TitleTickOverflow { title: title.id })?;
            max_title_id = max_title_id.max(title.id.0);
        }

        for transition in &mut snapshot.transitions {
            if transition.id.0 == 0 {
                return Err(TimelineSnapshotError::InvalidTransitionId(transition.id));
            }
            if !transition_ids.insert(transition.id) {
                return Err(TimelineSnapshotError::DuplicateTransitionId(transition.id));
            }
            if !transition.curve.is_finite() {
                return Err(TimelineSnapshotError::InvalidTransition {
                    transition: transition.id,
                });
            }
            transition.curve = transition.curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE);
            if !cuts.insert((
                transition.track_id,
                transition.left_clip,
                transition.right_clip,
            )) {
                return Err(TimelineSnapshotError::InvalidTransition {
                    transition: transition.id,
                });
            }
            max_transition_id = max_transition_id.max(transition.id.0);
        }

        for transition in &snapshot.audio_transitions {
            if transition.id.0 == 0 {
                return Err(TimelineSnapshotError::InvalidAudioTransitionId(
                    transition.id,
                ));
            }
            if !audio_transition_ids.insert(transition.id) {
                return Err(TimelineSnapshotError::DuplicateAudioTransitionId(
                    transition.id,
                ));
            }
            if !audio_cuts.insert((
                transition.track_id,
                transition.left_clip,
                transition.right_clip,
            )) {
                return Err(TimelineSnapshotError::InvalidAudioTransition {
                    transition: transition.id,
                });
            }
            max_audio_transition_id = max_audio_transition_id.max(transition.id.0);
        }

        let mut timeline = Self {
            tracks: snapshot.tracks,
            titles: snapshot.titles,
            transitions: snapshot.transitions,
            audio_transitions: snapshot.audio_transitions,
            clip_locations: HashMap::new(),
            next_track_id: next_id(max_track_id)?,
            next_clip_id: next_id(max_clip_id)?,
            next_link_id: next_id(max_link_id)?,
            next_title_id: next_id(max_title_id)?,
            next_transition_id: next_id(max_transition_id)?,
            next_audio_transition_id: next_id(max_audio_transition_id)?,
            generation: 1,
            structural_generation: 1,
        };
        timeline.rebuild_clip_locations();
        if timeline
            .transitions
            .iter()
            .any(|transition| !timeline.transition_is_valid(transition))
        {
            return Err(TimelineSnapshotError::InvalidTransition {
                transition: timeline
                    .transitions
                    .iter()
                    .find(|transition| !timeline.transition_is_valid(transition))
                    .expect("transition exists")
                    .id,
            });
        }
        if timeline
            .audio_transitions
            .iter()
            .any(|transition| !timeline.audio_transition_is_valid(transition))
        {
            return Err(TimelineSnapshotError::InvalidAudioTransition {
                transition: timeline
                    .audio_transitions
                    .iter()
                    .find(|transition| !timeline.audio_transition_is_valid(transition))
                    .expect("audio transition exists")
                    .id,
            });
        }
        Ok(timeline)
    }

    /// Monotonically changes once for every successful public edit that
    /// changes durable timeline state. Derived caches use this instead of an
    /// unrelated UI frame or project generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Changes only when the cache-visible track layout changes: tracks,
    /// clip identities, starts, or ends. Source-only and envelope edits leave
    /// this stable so the canvas retains its SoA columns.
    pub fn structural_generation(&self) -> u64 {
        self.structural_generation
    }

    fn apply_history_batch(&mut self, batch: &TimelineEditBatch, forward: bool) {
        if forward {
            for edit in &batch.edits {
                self.apply_history_edit(edit, true);
            }
        } else {
            for edit in batch.edits.iter().rev() {
                self.apply_history_edit(edit, false);
            }
        }
        for track in &mut self.tracks {
            track.clips.sort_by_key(|clip| clip.start);
        }
        self.rebuild_clip_locations();
        self.reseed_ids();
        debug_assert!(batch.structural || !batch.track_cache_structural);
        self.bump_generations(batch.track_cache_structural);
        debug_assert!(self.check_invariants().is_ok());
    }

    fn apply_history_edit(&mut self, edit: &TimelineEdit, forward: bool) {
        match (edit, forward) {
            (TimelineEdit::InsertTitle { title, index }, true)
            | (TimelineEdit::RemoveTitle { title, index }, false) => self
                .titles
                .insert((*index).min(self.titles.len()), title.clone()),
            (TimelineEdit::InsertTitle { title, .. }, false)
            | (TimelineEdit::RemoveTitle { title, .. }, true) => {
                if let Some(index) = self.titles.iter().position(|item| item.id == title.id) {
                    self.titles.remove(index);
                }
            }
            (TimelineEdit::PatchTitle { before, after }, direction) => {
                let replacement = if direction { after } else { before };
                if let Some(title) = self
                    .titles
                    .iter_mut()
                    .find(|title| title.id == replacement.id)
                {
                    *title = replacement.clone();
                }
            }
            (TimelineEdit::InsertClip { clip, track, index }, true)
            | (TimelineEdit::RemoveClip { clip, track, index }, false) => {
                if let Some(target) = self
                    .tracks
                    .iter_mut()
                    .find(|candidate| candidate.id == *track)
                {
                    target
                        .clips
                        .insert((*index).min(target.clips.len()), clip.clone());
                }
            }
            (TimelineEdit::InsertClip { clip, .. }, false)
            | (TimelineEdit::RemoveClip { clip, .. }, true) => {
                for track in &mut self.tracks {
                    if let Some(index) = track.clips.iter().position(|item| item.id == clip.id) {
                        track.clips.remove(index);
                        break;
                    }
                }
            }
            (TimelineEdit::PatchClip { before, after }, direction) => {
                let replacement = if direction { after } else { before };
                for track in &mut self.tracks {
                    if let Some(clip) = track
                        .clips
                        .iter_mut()
                        .find(|clip| clip.id == replacement.id)
                    {
                        *clip = replacement.clone();
                        break;
                    }
                }
            }
            (TimelineEdit::InsertTrack { track, index }, true)
            | (TimelineEdit::RemoveTrack { track, index }, false) => {
                self.tracks
                    .insert((*index).min(self.tracks.len()), track.clone());
            }
            (TimelineEdit::InsertTrack { track, .. }, false)
            | (TimelineEdit::RemoveTrack { track, .. }, true) => {
                if let Some(index) = self.tracks.iter().position(|item| item.id == track.id) {
                    self.tracks.remove(index);
                }
            }
            (TimelineEdit::PatchTrack { before, after }, direction) => {
                let replacement = if direction { after } else { before };
                if let Some(track) = self
                    .tracks
                    .iter_mut()
                    .find(|item| item.id == replacement.id)
                {
                    track.muted = replacement.muted;
                    track.solo = replacement.solo;
                    track.gain_db = replacement.gain_db;
                    track.pan = replacement.pan;
                    track.effects = replacement.effects.clone();
                }
            }
            (TimelineEdit::InsertTransition { transition, index }, true)
            | (TimelineEdit::RemoveTransition { transition, index }, false) => self
                .transitions
                .insert((*index).min(self.transitions.len()), transition.clone()),
            (TimelineEdit::InsertTransition { transition, .. }, false)
            | (TimelineEdit::RemoveTransition { transition, .. }, true) => {
                if let Some(index) = self
                    .transitions
                    .iter()
                    .position(|item| item.id == transition.id)
                {
                    self.transitions.remove(index);
                }
            }
            (TimelineEdit::PatchTransition { before, after }, direction) => {
                let replacement = if direction { after } else { before };
                if let Some(transition) = self
                    .transitions
                    .iter_mut()
                    .find(|item| item.id == replacement.id)
                {
                    *transition = replacement.clone();
                }
            }
            (TimelineEdit::InsertAudioTransition { transition, index }, true)
            | (TimelineEdit::RemoveAudioTransition { transition, index }, false) => {
                self.audio_transitions.insert(
                    (*index).min(self.audio_transitions.len()),
                    transition.clone(),
                )
            }
            (TimelineEdit::InsertAudioTransition { transition, .. }, false)
            | (TimelineEdit::RemoveAudioTransition { transition, .. }, true) => {
                if let Some(index) = self
                    .audio_transitions
                    .iter()
                    .position(|item| item.id == transition.id)
                {
                    self.audio_transitions.remove(index);
                }
            }
            (TimelineEdit::PatchAudioTransition { before, after }, direction) => {
                let replacement = if direction { after } else { before };
                if let Some(transition) = self
                    .audio_transitions
                    .iter_mut()
                    .find(|item| item.id == replacement.id)
                {
                    *transition = replacement.clone();
                }
            }
        }
    }

    fn reseed_ids(&mut self) {
        self.next_track_id = self
            .tracks
            .iter()
            .map(|track| track.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        self.next_clip_id = self
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .map(|clip| clip.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        self.next_link_id = self
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .filter_map(|clip| clip.link_id)
            .map(|link| link.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        self.next_title_id = self
            .titles
            .iter()
            .map(|title| title.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        self.next_transition_id = self
            .transitions
            .iter()
            .map(|transition| transition.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        self.next_audio_transition_id = self
            .audio_transitions
            .iter()
            .map(|transition| transition.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
    }

    pub fn titles(&self) -> &[TitleOverlay] {
        &self.titles
    }

    pub fn transitions(&self) -> &[VideoTransition] {
        &self.transitions
    }

    pub fn transition(&self, id: TransitionId) -> Option<&VideoTransition> {
        self.transitions
            .iter()
            .find(|transition| transition.id == id)
    }

    pub fn audio_transitions(&self) -> &[AudioTransition] {
        &self.audio_transitions
    }

    pub fn audio_transition(&self, id: AudioTransitionId) -> Option<&AudioTransition> {
        self.audio_transitions
            .iter()
            .find(|transition| transition.id == id)
    }

    /// Returns the centered active interval `[start, end)` for an audio transition.
    pub fn audio_transition_timing(&self, id: AudioTransitionId) -> Option<(Tick, Tick)> {
        let transition = self.audio_transition(id)?;
        let left = self.clip(transition.left_clip)?;
        let left_half = transition.duration.0 / 2;
        let start = Tick(left.end().0.checked_sub(left_half)?);
        let end = Tick(start.0.checked_add(transition.duration.0)?);
        Some((start, end))
    }

    pub fn active_audio_transition(&self, tick: Tick) -> Option<&AudioTransition> {
        self.audio_transitions.iter().find(|transition| {
            self.audio_transition_timing(transition.id)
                .is_some_and(|(start, end)| tick >= start && tick < end)
        })
    }

    /// Linear crossfade progress over the centered active interval, in `[0, 1]`.
    pub fn audio_transition_progress(&self, id: AudioTransitionId, tick: Tick) -> Option<f32> {
        let (start, end) = self.audio_transition_timing(id)?;
        if tick < start || tick >= end {
            return None;
        }
        Some((tick.0 - start.0) as f32 / (end.0 - start.0) as f32)
    }

    pub fn add_audio_transition(
        &mut self,
        track_id: TrackId,
        left_clip: ClipId,
        right_clip: ClipId,
        duration: Tick,
    ) -> Result<AudioTransitionId, TimelineError> {
        if self.next_audio_transition_id == 0 || self.next_audio_transition_id == u32::MAX {
            return Err(TimelineError::IdExhausted);
        }
        let transition = AudioTransition {
            id: AudioTransitionId(self.next_audio_transition_id),
            track_id,
            left_clip,
            right_clip,
            duration,
            kind: AudioTransitionKind::EqualPowerCrossfade,
        };
        self.validate_audio_transition(&transition, None)?;
        self.next_audio_transition_id += 1;
        self.audio_transitions.push(transition);
        self.bump_generations(false);
        Ok(AudioTransitionId(self.next_audio_transition_id - 1))
    }

    pub fn replace_audio_transition(
        &mut self,
        id: AudioTransitionId,
        duration: Tick,
    ) -> Result<(), TimelineError> {
        let mut replacement = self
            .audio_transition(id)
            .cloned()
            .ok_or(TimelineError::UnknownAudioTransition(id))?;
        replacement.duration = duration;
        self.validate_audio_transition(&replacement, Some(id))?;
        let transition = self
            .audio_transitions
            .iter_mut()
            .find(|item| item.id == id)
            .expect("audio transition was found above");
        if *transition != replacement {
            *transition = replacement;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn remove_audio_transition(
        &mut self,
        id: AudioTransitionId,
    ) -> Result<AudioTransition, TimelineError> {
        let index = self
            .audio_transitions
            .iter()
            .position(|item| item.id == id)
            .ok_or(TimelineError::UnknownAudioTransition(id))?;
        let transition = self.audio_transitions.remove(index);
        self.bump_generations(false);
        Ok(transition)
    }

    /// Returns the centered active interval `[start, end)` for a transition.
    pub fn transition_timing(&self, id: TransitionId) -> Option<(Tick, Tick)> {
        let transition = self.transition(id)?;
        let left = self.clip(transition.left_clip)?;
        let left_half = transition.duration.0 / 2;
        let start = Tick(left.end().0.checked_sub(left_half)?);
        let end = Tick(start.0.checked_add(transition.duration.0)?);
        Some((start, end))
    }

    pub fn active_transition(&self, tick: Tick) -> Option<&VideoTransition> {
        self.transitions.iter().find(|transition| {
            self.transition_timing(transition.id)
                .is_some_and(|(start, end)| tick >= start && tick < end)
        })
    }

    /// Linear transition progress over the centered active interval, in `[0, 1]`.
    pub fn transition_progress(&self, id: TransitionId, tick: Tick) -> Option<f32> {
        let (start, end) = self.transition_timing(id)?;
        if tick < start || tick >= end {
            return None;
        }
        Some((tick.0 - start.0) as f32 / (end.0 - start.0) as f32)
    }

    pub fn add_video_transition(
        &mut self,
        track_id: TrackId,
        left_clip: ClipId,
        right_clip: ClipId,
        duration: Tick,
        curve: f32,
    ) -> Result<TransitionId, TimelineError> {
        self.add_video_transition_of_kind(
            track_id,
            left_clip,
            right_clip,
            duration,
            curve,
            VideoTransitionKind::CrossDissolve,
        )
    }

    pub fn add_video_transition_of_kind(
        &mut self,
        track_id: TrackId,
        left_clip: ClipId,
        right_clip: ClipId,
        duration: Tick,
        curve: f32,
        kind: VideoTransitionKind,
    ) -> Result<TransitionId, TimelineError> {
        if self.next_transition_id == 0 || self.next_transition_id == u32::MAX {
            return Err(TimelineError::IdExhausted);
        }
        let id = TransitionId(self.next_transition_id);
        if !curve.is_finite() {
            return Err(TimelineError::InvalidTransition);
        }
        let transition = VideoTransition {
            id,
            track_id,
            left_clip,
            right_clip,
            duration,
            curve: curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE),
            kind,
        };
        self.validate_transition(&transition, None)?;
        self.next_transition_id += 1;
        self.transitions.push(transition);
        self.bump_generations(false);
        Ok(id)
    }

    pub fn add_transition(
        &mut self,
        track_id: TrackId,
        left_clip: ClipId,
        right_clip: ClipId,
        duration: Tick,
        curve: f32,
    ) -> Result<TransitionId, TimelineError> {
        self.add_video_transition(track_id, left_clip, right_clip, duration, curve)
    }

    pub fn replace_video_transition(
        &mut self,
        id: TransitionId,
        mut replacement: VideoTransition,
    ) -> Result<(), TimelineError> {
        if replacement.id != id {
            return Err(TimelineError::InvalidTransition);
        }
        if !replacement.curve.is_finite() {
            return Err(TimelineError::InvalidTransition);
        }
        replacement.curve = replacement.curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE);
        self.validate_transition(&replacement, Some(id))?;
        let transition = self
            .transitions
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(TimelineError::UnknownTransition(id))?;
        if *transition != replacement {
            *transition = replacement;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn replace_transition(
        &mut self,
        id: TransitionId,
        replacement: VideoTransition,
    ) -> Result<(), TimelineError> {
        self.replace_video_transition(id, replacement)
    }

    pub fn remove_video_transition(
        &mut self,
        id: TransitionId,
    ) -> Result<VideoTransition, TimelineError> {
        let index = self
            .transitions
            .iter()
            .position(|item| item.id == id)
            .ok_or(TimelineError::UnknownTransition(id))?;
        let transition = self.transitions.remove(index);
        self.bump_generations(false);
        Ok(transition)
    }

    pub fn remove_transition(
        &mut self,
        id: TransitionId,
    ) -> Result<VideoTransition, TimelineError> {
        self.remove_video_transition(id)
    }

    pub fn title(&self, id: TitleId) -> Option<&TitleOverlay> {
        self.titles.iter().find(|title| title.id == id)
    }

    pub fn add_title(
        &mut self,
        start: Tick,
        duration: Tick,
        text: impl Into<String>,
    ) -> Result<TitleId, TimelineError> {
        if self.next_title_id == 0
            || self.next_title_id == u32::MAX
            || self
                .titles
                .iter()
                .any(|title| title.id.0 == self.next_title_id)
        {
            return Err(TimelineError::IdExhausted);
        }
        let id = TitleId(self.next_title_id);
        let mut title = TitleOverlay::new(id, start, duration, text.into());
        if !Self::normalize_title(&mut title)
            || title.start.0.checked_add(title.duration.0).is_none()
        {
            return Err(TimelineError::InvalidTitle);
        }
        self.next_title_id += 1;
        self.titles.push(title);
        self.bump_generations(false);
        Ok(id)
    }

    pub fn replace_title(
        &mut self,
        id: TitleId,
        mut replacement: TitleOverlay,
    ) -> Result<(), TimelineError> {
        if replacement.id != id {
            return Err(TimelineError::TitleIdMismatch {
                expected: id,
                actual: replacement.id,
            });
        }
        if !Self::normalize_title(&mut replacement)
            || replacement
                .start
                .0
                .checked_add(replacement.duration.0)
                .is_none()
        {
            return Err(TimelineError::InvalidTitle);
        }
        let title = self
            .titles
            .iter_mut()
            .find(|title| title.id == id)
            .ok_or(TimelineError::UnknownTitle(id))?;
        if *title != replacement {
            *title = replacement;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn remove_title(&mut self, id: TitleId) -> Result<TitleOverlay, TimelineError> {
        let index = self
            .titles
            .iter()
            .position(|title| title.id == id)
            .ok_or(TimelineError::UnknownTitle(id))?;
        let title = self.titles.remove(index);
        self.bump_generations(false);
        Ok(title)
    }

    pub fn active_titles(&self, tick: Tick) -> Vec<&TitleOverlay> {
        let mut titles = self
            .titles
            .iter()
            .filter(|title| {
                title.enabled
                    && tick >= title.start
                    && tick.0 < title.start.0.saturating_add(title.duration.0)
            })
            .collect::<Vec<_>>();
        titles.sort_by_key(|title| (title.z_order, title.start, title.id));
        titles
    }

    fn normalize_title(title: &mut TitleOverlay) -> bool {
        if title.start.0 < 0
            || title.duration.0 <= 0
            || title.text.len() > 16 * 1024
            || title.text.trim().is_empty()
            || !title.font_size.is_finite()
            || !(4.0..=512.0).contains(&title.font_size)
            || !title.position_x.is_finite()
            || !(0.0..=1.0).contains(&title.position_x)
            || !title.position_y.is_finite()
            || !(0.0..=1.0).contains(&title.position_y)
            || !title.outline_width.is_finite()
            || !(0.0..=32.0).contains(&title.outline_width)
            || !title.shadow_offset_x.is_finite()
            || !(-256.0..=256.0).contains(&title.shadow_offset_x)
            || !title.shadow_offset_y.is_finite()
            || !(-256.0..=256.0).contains(&title.shadow_offset_y)
            || !title.shadow_blur.is_finite()
            || !(0.0..=64.0).contains(&title.shadow_blur)
            || !title.opacity.is_finite()
            || !(0.0..=1.0).contains(&title.opacity)
            || title.fade_in.0 < 0
            || title.fade_out.0 < 0
        {
            return false;
        }
        title.fade_in.0 = title.fade_in.0.min(title.duration.0);
        title.fade_out.0 = title.fade_out.0.min(title.duration.0 - title.fade_in.0);
        true
    }

    /// Number of clips in the authoritative timeline. The edit index already owns one entry per
    /// clip, so diagnostics can read this in constant time without walking a large project.
    pub fn clip_count(&self) -> usize {
        self.clip_locations.len()
    }

    pub fn add_track(&mut self, kind: TrackKind) -> TrackId {
        let id = TrackId(self.next_track_id);
        self.next_track_id = self.next_track_id.saturating_add(1);
        self.tracks.push(Track {
            id,
            kind,
            muted: false,
            solo: false,
            gain_db: 0.0,
            pan: 0.0,
            effects: Vec::new(),
            clips: Vec::new(),
        });
        self.bump_generations(true);
        id
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|track| track.id == id)
    }

    pub fn set_track_muted(&mut self, id: TrackId, muted: bool) -> Result<(), TimelineError> {
        let track = self
            .tracks
            .iter_mut()
            .find(|track| track.id == id)
            .ok_or(TimelineError::UnknownTrack(id))?;
        if track.muted != muted {
            track.muted = muted;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_track_solo(&mut self, id: TrackId, solo: bool) -> Result<(), TimelineError> {
        let track = self.audio_track_mut(id)?;
        if track.solo != solo {
            track.solo = solo;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_track_audio_gain(&mut self, id: TrackId, gain_db: f32) -> Result<(), TimelineError> {
        if !gain_db.is_finite() {
            return Err(TimelineError::NonFiniteAudioControl);
        }
        let gain_db = gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        let track = self.audio_track_mut(id)?;
        if track.gain_db != gain_db {
            track.gain_db = gain_db;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_track_pan(&mut self, id: TrackId, pan: f32) -> Result<(), TimelineError> {
        if !pan.is_finite() {
            return Err(TimelineError::NonFiniteAudioControl);
        }
        let pan = pan.clamp(MIN_PAN, MAX_PAN);
        let track = self.audio_track_mut(id)?;
        if track.pan != pan {
            track.pan = pan;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_track_audio_effects(
        &mut self,
        id: TrackId,
        effects: Vec<AudioEffect>,
    ) -> Result<(), TimelineError> {
        if effects.len() > MAX_AUDIO_EFFECTS_PER_SCOPE
            || effects.iter().any(|effect| !effect.is_valid())
        {
            return Err(TimelineError::InvalidAudioEffect);
        }
        let track = self.audio_track_mut(id)?;
        if track.effects != effects {
            track.effects = effects;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clip_location(id)
            .and_then(|location| {
                self.tracks
                    .get(location.track_index)?
                    .clips
                    .get(location.clip_index)
            })
            .or_else(|| {
                // `tracks` remains public for transition compatibility. A
                // malformed external test may temporarily stale the index.
                self.tracks
                    .iter()
                    .flat_map(|track| &track.clips)
                    .find(|clip| clip.id == id)
            })
    }

    pub fn insert_clip(
        &mut self,
        track_id: TrackId,
        media: MediaId,
        start: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<ClipId, TimelineError> {
        let id = self.insert_clip_with_link(track_id, media, start, duration, source_in, None)?;
        self.bump_generations(true);
        Ok(id)
    }

    /// Inserts matching video and audio clips on the first tracks of each kind.
    /// The two clips share only their link ID; gain and fades remain independent.
    pub fn insert_linked_av_pair(
        &mut self,
        media: MediaId,
        start: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<LinkedClipPair, TimelineError> {
        let video_track = self.first_track_of_kind(TrackKind::Video)?;
        let audio_track = self.first_track_of_kind(TrackKind::Audio)?;
        let link_id = self.allocate_link_id();
        let video = self.insert_clip_with_link(
            video_track,
            media,
            start,
            duration,
            source_in,
            Some(link_id),
        )?;
        match self.insert_clip_with_link(
            audio_track,
            media,
            start,
            duration,
            source_in,
            Some(link_id),
        ) {
            Ok(audio) => {
                self.bump_generations(true);
                Ok(LinkedClipPair {
                    link_id,
                    video,
                    audio,
                })
            }
            Err(error) => {
                self.remove_clip(video);
                Err(error)
            }
        }
    }

    /// Splits the clip containing `at`. Exact clip boundaries are not edits.
    pub fn razor(
        &mut self,
        track_id: TrackId,
        at: Tick,
    ) -> Result<Option<RazorSplit>, TimelineError> {
        let track_index = self.track_index(track_id)?;
        let clip_index = self.tracks[track_index]
            .clips
            .iter()
            .position(|clip| clip.start < at && at < clip.end());
        let Some(clip_index) = clip_index else {
            return Ok(None);
        };

        let original = self.tracks[track_index].clips[clip_index].clone();
        let left_duration = Tick(at.0 - original.start.0);
        let right_duration = Tick(original.end().0 - at.0);
        let right_source_in = checked_add(original.source_in, left_duration)?;
        let right_id = self.allocate_clip_id();

        let mut left = original.clone();
        left.duration = left_duration;
        left.fade_out = Fade::default();
        Self::clamp_fades_to_clip(&mut left);

        let mut right = original;
        right.id = right_id;
        right.start = at;
        right.duration = right_duration;
        right.source_in = right_source_in;
        right.fade_in = Fade::default();
        Self::clamp_fades_to_clip(&mut right);

        let track = &mut self.tracks[track_index];
        track.clips[clip_index] = left.clone();
        track.clips.insert(clip_index + 1, right);
        debug_assert!(Self::track_is_valid(track));
        self.rebuild_clip_locations();
        self.bump_generations(true);
        Ok(Some(RazorSplit {
            left: left.id,
            right: right_id,
        }))
    }

    /// Splits the clip under `at` and every linked counterpart covering the
    /// same tick. Linkage controls structural edits only; each resulting
    /// section keeps its own independent gain and fade envelopes.
    pub fn razor_linked(
        &mut self,
        track_id: TrackId,
        at: Tick,
    ) -> Result<Vec<RazorSplit>, TimelineError> {
        self.atomic_edit(|timeline| {
            let track_index = timeline.track_index(track_id)?;
            let target = timeline.tracks[track_index]
                .clips
                .iter()
                .find(|clip| clip.start < at && at < clip.end());
            let Some(target) = target else {
                return Ok(Vec::new());
            };
            let link_id = target.link_id;
            let tracks: Vec<_> = timeline
                .tracks
                .iter()
                .filter(|track| {
                    track.id == track_id
                        || link_id.is_some_and(|link_id| {
                            track.clips.iter().any(|clip| {
                                clip.link_id == Some(link_id) && clip.start < at && at < clip.end()
                            })
                        })
                })
                .map(|track| track.id)
                .collect();
            let mut splits = Vec::with_capacity(tracks.len());
            for track in tracks {
                if let Some(split) = timeline.razor(track, at)? {
                    splits.push(split);
                }
            }
            Ok(splits)
        })
    }

    pub fn set_audio_gain(&mut self, clip_id: ClipId, gain_db: f32) -> Result<(), TimelineError> {
        if !gain_db.is_finite() {
            return Err(TimelineError::NonFiniteAudioControl);
        }
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Audio {
            return Err(TimelineError::AudioOnly(clip_id));
        }
        let gain_db = gain_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        if clip.gain_db != gain_db {
            clip.gain_db = gain_db;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_audio_channel_gain(
        &mut self,
        clip_id: ClipId,
        gain_left_db: f32,
        gain_right_db: f32,
    ) -> Result<(), TimelineError> {
        if !gain_left_db.is_finite() || !gain_right_db.is_finite() {
            return Err(TimelineError::NonFiniteAudioControl);
        }
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Audio {
            return Err(TimelineError::AudioOnly(clip_id));
        }
        let gain_left_db = gain_left_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        let gain_right_db = gain_right_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        if clip.gain_left_db != gain_left_db || clip.gain_right_db != gain_right_db {
            clip.gain_left_db = gain_left_db;
            clip.gain_right_db = gain_right_db;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_clip_audio_effects(
        &mut self,
        clip_id: ClipId,
        effects: Vec<AudioEffect>,
    ) -> Result<(), TimelineError> {
        if effects.len() > MAX_AUDIO_EFFECTS_PER_SCOPE
            || effects.iter().any(|effect| !effect.is_valid())
        {
            return Err(TimelineError::InvalidAudioEffect);
        }
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Audio {
            return Err(TimelineError::AudioOnly(clip_id));
        }
        if clip.effects != effects {
            clip.effects = effects;
            self.bump_generations(false);
        }
        Ok(())
    }

    /// Enables or disables `clip_id` and, when requested, its exact linked
    /// counterparts (same link, start, and duration). The selection is
    /// resolved before any mutation so a missing target cannot leave a partial
    /// linked edit behind. Returns only clips whose durable state changed.
    pub fn set_clip_enabled(
        &mut self,
        clip_id: ClipId,
        enabled: bool,
        linked_selection: bool,
    ) -> Result<Vec<ClipId>, TimelineError> {
        let selected = self.selected_clips(clip_id, linked_selection)?;
        let changed = selected
            .iter()
            .filter(|clip| clip.enabled != enabled)
            .map(|clip| clip.id)
            .collect::<Vec<_>>();
        if changed.is_empty() {
            return Ok(changed);
        }
        for id in &changed {
            self.clip_mut(*id)?.1.enabled = enabled;
        }
        self.bump_generations(false);
        Ok(changed)
    }

    pub fn set_clip_transform(
        &mut self,
        clip_id: ClipId,
        transform: ClipTransform,
    ) -> Result<(), TimelineError> {
        if !transform.is_finite() {
            return Err(TimelineError::NonFiniteTransform);
        }
        let (_, clip) = self.clip_mut(clip_id)?;
        let transform = transform.clamped();
        if clip.transform != transform {
            clip.transform = transform;
            self.bump_generations(false);
        }
        Ok(())
    }

    /// Replaces the clip's video-effect stack after validating durable data.
    pub fn set_clip_video_effects(
        &mut self,
        clip_id: ClipId,
        mut effects: Vec<VideoEffectNode>,
    ) -> Result<(), TimelineError> {
        if !normalize_video_effects(&mut effects) {
            return Err(TimelineError::InvalidVideoEffect);
        }
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Video {
            return Err(TimelineError::VideoOnly(clip_id));
        }
        if clip.video_effects != effects {
            clip.video_effects = effects;
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_color_parameter(
        &mut self,
        clip_id: ClipId,
        effect_id: VideoEffectId,
        parameter: ColorParameter,
        value: f32,
    ) -> Result<(), TimelineError> {
        if !value.is_finite() {
            return Err(TimelineError::InvalidVideoEffect);
        }
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Video {
            return Err(TimelineError::VideoOnly(clip_id));
        }
        let scalar = color_scalar_mut(clip, effect_id, parameter)
            .ok_or(TimelineError::InvalidVideoEffect)?;
        let value = color_parameter_value(parameter, value);
        if scalar.value != value {
            scalar.value = value;
            self.bump_generations(false);
        }
        Ok(())
    }

    /// Inserts or replaces a source-time keyframe. The key is not shifted by
    /// razor or trim because its coordinate belongs to the source media.
    pub fn set_color_keyframe(
        &mut self,
        clip_id: ClipId,
        effect_id: VideoEffectId,
        parameter: ColorParameter,
        source_tick: Tick,
        value: f32,
        interpolation: KeyframeInterpolation,
    ) -> Result<(), TimelineError> {
        if source_tick.0 < 0 || !value.is_finite() {
            return Err(TimelineError::InvalidVideoEffect);
        }
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Video {
            return Err(TimelineError::VideoOnly(clip_id));
        }
        let scalar = color_scalar_mut(clip, effect_id, parameter)
            .ok_or(TimelineError::InvalidVideoEffect)?;
        let value = color_parameter_value(parameter, value);
        let changed = match scalar
            .keyframes
            .binary_search_by_key(&source_tick, |key| key.source_tick)
        {
            Ok(index) => {
                let replacement = ScalarKeyframe {
                    source_tick,
                    value,
                    interpolation,
                };
                if scalar.keyframes[index] == replacement {
                    false
                } else {
                    scalar.keyframes[index] = replacement;
                    true
                }
            }
            Err(index) if scalar.keyframes.len() < MAX_KEYFRAMES_PER_PARAMETER => {
                scalar.keyframes.insert(
                    index,
                    ScalarKeyframe {
                        source_tick,
                        value,
                        interpolation,
                    },
                );
                true
            }
            Err(_) => return Err(TimelineError::InvalidVideoEffect),
        };
        if changed {
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn remove_color_keyframe(
        &mut self,
        clip_id: ClipId,
        effect_id: VideoEffectId,
        parameter: ColorParameter,
        source_tick: Tick,
    ) -> Result<bool, TimelineError> {
        let (track_kind, clip) = self.clip_mut(clip_id)?;
        if track_kind != TrackKind::Video {
            return Err(TimelineError::VideoOnly(clip_id));
        }
        let scalar = color_scalar_mut(clip, effect_id, parameter)
            .ok_or(TimelineError::InvalidVideoEffect)?;
        let Ok(index) = scalar
            .keyframes
            .binary_search_by_key(&source_tick, |key| key.source_tick)
        else {
            return Ok(false);
        };
        scalar.keyframes.remove(index);
        self.bump_generations(false);
        Ok(true)
    }

    pub fn color_keyframe(
        &self,
        clip_id: ClipId,
        effect_id: VideoEffectId,
        parameter: ColorParameter,
        source_tick: Tick,
    ) -> Option<&ScalarKeyframe> {
        let clip = self.clip(clip_id)?;
        let scalar = color_scalar(clip, effect_id, parameter)?;
        scalar
            .keyframes
            .binary_search_by_key(&source_tick, |key| key.source_tick)
            .ok()
            .and_then(|index| scalar.keyframes.get(index))
    }

    pub fn set_fade_duration(
        &mut self,
        clip_id: ClipId,
        edge: FadeEdge,
        duration: Tick,
    ) -> Result<(), TimelineError> {
        let (_, clip) = self.clip_mut(clip_id)?;
        let requested = duration.0.max(0);
        let previous = match edge {
            FadeEdge::In => clip.fade_in.duration,
            FadeEdge::Out => clip.fade_out.duration,
        };
        match edge {
            FadeEdge::In => {
                clip.fade_in.duration =
                    Tick(requested.min((clip.duration.0 - clip.fade_out.duration.0).max(0)));
            }
            FadeEdge::Out => {
                clip.fade_out.duration =
                    Tick(requested.min((clip.duration.0 - clip.fade_in.duration.0).max(0)));
            }
        }
        let changed = match edge {
            FadeEdge::In => clip.fade_in.duration != previous,
            FadeEdge::Out => clip.fade_out.duration != previous,
        };
        if changed {
            self.bump_generations(false);
        }
        Ok(())
    }

    pub fn set_fade_curve(
        &mut self,
        clip_id: ClipId,
        edge: FadeEdge,
        curve: f32,
    ) -> Result<(), TimelineError> {
        let (_, clip) = self.clip_mut(clip_id)?;
        let previous = match edge {
            FadeEdge::In => clip.fade_in.curve,
            FadeEdge::Out => clip.fade_out.curve,
        };
        match edge {
            FadeEdge::In => clip.fade_in.curve = curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE),
            FadeEdge::Out => clip.fade_out.curve = curve.clamp(MIN_FADE_CURVE, MAX_FADE_CURVE),
        }
        let changed = match edge {
            FadeEdge::In => clip.fade_in.curve != previous,
            FadeEdge::Out => clip.fade_out.curve != previous,
        };
        if changed {
            self.bump_generations(false);
        }
        Ok(())
    }

    /// Moves a clip and its directly linked counterparts by `delta`.
    ///
    /// A linked section is identified by its link ID plus its original start
    /// and duration. This keeps independently split sections from being
    /// conflated. The whole move is preflighted before any clip is changed.
    pub fn move_clip(&mut self, clip_id: ClipId, delta: Tick) -> Result<(), TimelineError> {
        self.move_clip_with_link(clip_id, delta, true)
    }

    /// Moves only `clip_id` when `linked_selection` is false; when true, also
    /// moves the exact matching A/V section (same link, start, and duration).
    /// The edit is atomic: on error the timeline remains unchanged.
    pub fn move_clip_with_link(
        &mut self,
        clip_id: ClipId,
        delta: Tick,
        linked_selection: bool,
    ) -> Result<(), TimelineError> {
        let selected = self.selected_clips(clip_id, linked_selection)?;
        let mut changes = Vec::with_capacity(selected.len());
        for clip in &selected {
            let start = checked_add(clip.start, delta)?;
            if start.0 < 0 {
                return Err(TimelineError::NegativeStart { clip: clip.id });
            }
            changes.push(TimingChange {
                id: clip.id,
                start,
                duration: clip.duration,
                source_in: clip.source_in,
            });
        }
        self.validate_timing_changes(&changes)?;
        if changes.iter().all(|change| {
            self.clip(change.id)
                .is_some_and(|clip| clip.start == change.start)
        }) {
            return Ok(());
        }
        self.apply_timing_changes(&changes);
        self.bump_generations(true);
        Ok(())
    }

    /// Trims a clip's start. Positive `delta` removes material from its start;
    /// negative `delta` extends it. With ripple enabled, later clips on each
    /// affected track shift by `-delta`; without ripple the end stays fixed.
    pub fn trim_start(
        &mut self,
        clip_id: ClipId,
        delta: Tick,
        linked_selection: bool,
        ripple: bool,
    ) -> Result<(), TimelineError> {
        self.trim(clip_id, delta, linked_selection, ripple, FadeEdge::In)
    }

    /// Trims a clip's end. Positive `delta` extends the end; negative `delta`
    /// shortens it. With ripple enabled, later clips shift by `delta`.
    pub fn trim_end(
        &mut self,
        clip_id: ClipId,
        delta: Tick,
        linked_selection: bool,
        ripple: bool,
    ) -> Result<(), TimelineError> {
        self.trim(clip_id, delta, linked_selection, ripple, FadeEdge::Out)
    }

    /// Changes source content without moving or resizing a timeline section.
    pub fn slip_clip(
        &mut self,
        clip_id: ClipId,
        source_delta: Tick,
        linked_selection: bool,
    ) -> Result<(), TimelineError> {
        let selected = self.selected_clips(clip_id, linked_selection)?;
        let mut changes = Vec::with_capacity(selected.len());
        for clip in &selected {
            let source_in = checked_add(clip.source_in, source_delta)?;
            if source_in.0 < 0 {
                return Err(TimelineError::NegativeSourceIn { clip: clip.id });
            }
            changes.push((clip.id, source_in));
        }
        if changes.iter().all(|(id, source_in)| {
            self.clip(*id)
                .is_some_and(|clip| clip.source_in == *source_in)
        }) {
            return Ok(());
        }
        for (id, source_in) in changes {
            self.clip_mut(id)?.1.source_in = source_in;
        }
        self.bump_generations(false);
        Ok(())
    }

    /// Removes a clip and, when requested, its exact linked A/V counterpart.
    /// The removed records are returned for selection repair and command history.
    pub fn delete_clip(
        &mut self,
        clip_id: ClipId,
        linked_selection: bool,
    ) -> Result<Vec<Clip>, TimelineError> {
        self.atomic_edit(|timeline| {
            let selected = timeline.selected_clips(clip_id, linked_selection)?;
            for clip in &selected {
                timeline.remove_clip(clip.id);
            }
            Ok(selected)
        })
    }

    /// Creates room at `at` on V1/A1 or A1, splitting any occupied section at
    /// that point and rippling later clips. New video/audio clips are linked.
    pub fn insert_edit(
        &mut self,
        target: EditTarget,
        media: MediaId,
        at: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<Vec<ClipId>, TimelineError> {
        self.atomic_edit(|timeline| {
            timeline.insert_edit_in_place(target, media, at, duration, source_in)
        })
    }

    /// Replaces `[at, at + duration)` on V1/A1 or A1. Existing sections are
    /// cut into left/right tails, retaining their source offsets and outer
    /// fades. This never ripples later material.
    pub fn overwrite_edit(
        &mut self,
        target: EditTarget,
        media: MediaId,
        at: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<Vec<ClipId>, TimelineError> {
        self.atomic_edit(|timeline| {
            timeline.overwrite_edit_in_place(target, media, at, duration, source_in)
        })
    }

    /// Replaces the source media while preserving the selected section's exact
    /// position and duration. A linked exact counterpart is optional.
    pub fn replace_clip_media(
        &mut self,
        clip_id: ClipId,
        media: MediaId,
        source_in: Tick,
        linked_selection: bool,
    ) -> Result<(), TimelineError> {
        if source_in.0 < 0 {
            return Err(TimelineError::NegativeSourceIn { clip: clip_id });
        }
        self.atomic_edit(|timeline| {
            for clip in timeline.selected_clips(clip_id, linked_selection)? {
                let selected = timeline.clip_mut(clip.id)?.1;
                selected.media = media;
                selected.source_in = source_in;
            }
            Ok(())
        })
    }

    /// Rolls the shared boundary between adjacent `left_clip` and `right_clip`.
    /// Positive `boundary_delta` gives time to the left section and takes it
    /// from the right; the pair's combined timeline span remains fixed.
    /// When selected, the exact matching linked A/V boundary is rolled too.
    pub fn roll_edit(
        &mut self,
        left_clip: ClipId,
        right_clip: ClipId,
        boundary_delta: Tick,
        linked_selection: bool,
    ) -> Result<(), TimelineError> {
        let left = self
            .clip(left_clip)
            .cloned()
            .ok_or(TimelineError::UnknownClip(left_clip))?;
        let right = self
            .clip(right_clip)
            .cloned()
            .ok_or(TimelineError::UnknownClip(right_clip))?;
        if left.track_id != right.track_id || left.end() != right.start {
            return Err(TimelineError::RollNotAdjacent {
                left: left_clip,
                right: right_clip,
            });
        }

        let mut pairs = vec![(left.id, right.id)];
        if linked_selection && left.link_id.is_some() && right.link_id.is_some() {
            for track in &self.tracks {
                let left_index = track.clips.partition_point(|clip| clip.start < left.start);
                let right_index = track.clips.partition_point(|clip| clip.start < right.start);
                let (Some(candidate_left), Some(candidate_right)) =
                    (track.clips.get(left_index), track.clips.get(right_index))
                else {
                    continue;
                };
                let matches_left = candidate_left.link_id == left.link_id
                    && candidate_left.start == left.start
                    && candidate_left.duration == left.duration;
                let matches_right = candidate_right.link_id == right.link_id
                    && candidate_right.start == right.start
                    && candidate_right.duration == right.duration;
                if matches_left
                    && matches_right
                    && !pairs.contains(&(candidate_left.id, candidate_right.id))
                {
                    pairs.push((candidate_left.id, candidate_right.id));
                }
            }
        }

        let mut changes = Vec::with_capacity(pairs.len() * 2);
        for &(left_id, right_id) in &pairs {
            let left = self.clip(left_id).expect("selected clip exists");
            let right = self.clip(right_id).expect("selected clip exists");
            let left_duration = checked_add(left.duration, boundary_delta)?;
            let right_duration = checked_add(right.duration, checked_neg(boundary_delta)?)?;
            let right_source_in = checked_add(right.source_in, boundary_delta)?;
            if left_duration.0 <= 0 || right_duration.0 <= 0 {
                return Err(TimelineError::InvalidDuration);
            }
            if right_source_in.0 < 0 {
                return Err(TimelineError::NegativeSourceIn { clip: right_id });
            }
            changes.push(TimingChange {
                id: left_id,
                start: left.start,
                duration: left_duration,
                source_in: left.source_in,
            });
            changes.push(TimingChange {
                id: right_id,
                start: checked_add(right.start, boundary_delta)?,
                duration: right_duration,
                source_in: right_source_in,
            });
        }
        self.validate_timing_changes(&changes)?;
        if boundary_delta.0 == 0 {
            return Ok(());
        }
        self.apply_timing_changes(&changes);
        self.bump_generations(true);
        Ok(())
    }

    /// Reconciles placed clips when probing reveals a source's true duration.
    ///
    /// This operation only shortens clips: a user trim or the provisional
    /// placement length is never extended by a probe result. Changes are
    /// preflighted and applied atomically, preserving track order and the
    /// no-overlap rule.
    pub fn clamp_media_duration(
        &mut self,
        media: MediaId,
        media_duration: Tick,
    ) -> Result<usize, TimelineError> {
        if media_duration.0 <= 0 {
            return Err(TimelineError::InvalidMediaDuration);
        }

        let mut replacements = Vec::new();
        for (track_index, track) in self.tracks.iter().enumerate() {
            for (clip_index, clip) in track.clips.iter().enumerate() {
                if clip.media != media {
                    continue;
                }
                let available = media_duration.0 - clip.source_in.0;
                if available <= 0 {
                    return Err(TimelineError::SourceOutsideMedia {
                        clip: clip.id,
                        media,
                    });
                }
                let duration = Tick(clip.duration.0.min(available));
                if duration != clip.duration {
                    replacements.push((track_index, clip_index, duration));
                }
            }
        }

        // Shortening cannot create an overlap, but keep this explicit check so
        // future duration reconciliation remains safely transactional.
        for &(track_index, clip_index, duration) in &replacements {
            let track = &self.tracks[track_index];
            let new_end = checked_add(track.clips[clip_index].start, duration)?;
            if track
                .clips
                .get(clip_index + 1)
                .is_some_and(|next| new_end > next.start)
            {
                return Err(TimelineError::InvariantViolation);
            }
        }

        for (track_index, clip_index, duration) in &replacements {
            let clip = &mut self.tracks[*track_index].clips[*clip_index];
            clip.duration = *duration;
            Self::clamp_fades_to_clip(clip);
        }
        if !replacements.is_empty() {
            self.bump_generations(true);
        }
        Ok(replacements.len())
    }

    /// Replaces the provisional duration of explicitly owned clips with the source duration.
    ///
    /// Unlike [`Self::clamp_media_duration`], this may extend a clip. Extension stops at the
    /// next occupied section on the same track, so late probe results never overwrite another
    /// edit. Callers must pass only clips whose duration is still probe-owned.
    pub fn reconcile_provisional_media_duration(
        &mut self,
        media: MediaId,
        clip_ids: &[ClipId],
        media_duration: Tick,
    ) -> Result<usize, TimelineError> {
        if media_duration.0 <= 0 {
            return Err(TimelineError::InvalidMediaDuration);
        }

        let mut replacements = Vec::new();
        for &clip_id in clip_ids {
            let Some(location) = self.clip_location(clip_id) else {
                continue;
            };
            let track = &self.tracks[location.track_index];
            let clip = &track.clips[location.clip_index];
            if clip.media != media {
                continue;
            }
            let available = media_duration.0 - clip.source_in.0;
            if available <= 0 {
                return Err(TimelineError::SourceOutsideMedia {
                    clip: clip.id,
                    media,
                });
            }
            let room = track
                .clips
                .get(location.clip_index + 1)
                .map_or(i64::MAX, |next| next.start.0 - clip.start.0);
            let duration = Tick(available.min(room));
            if duration.0 <= 0 {
                return Err(TimelineError::InvariantViolation);
            }
            if duration != clip.duration {
                replacements.push((location.track_index, location.clip_index, duration));
            }
        }

        for &(track_index, clip_index, duration) in &replacements {
            let clip = &mut self.tracks[track_index].clips[clip_index];
            clip.duration = duration;
            Self::clamp_fades_to_clip(clip);
        }
        if !replacements.is_empty() {
            self.bump_generations(true);
        }
        debug_assert!(self.check_invariants().is_ok());
        Ok(replacements.len())
    }

    /// Checks the ordered, non-overlapping compact-array invariant.
    pub fn check_invariants(&self) -> Result<(), TimelineError> {
        let mut title_ids = HashSet::with_capacity(self.titles.len());
        let titles_are_valid = self.titles.iter().all(|title| {
            let mut normalized = title.clone();
            title.id.0 != 0
                && title_ids.insert(title.id)
                && Self::normalize_title(&mut normalized)
                && normalized == *title
                && title.start.0.checked_add(title.duration.0).is_some()
        });
        let mut transition_ids = HashSet::with_capacity(self.transitions.len());
        let mut cuts = HashSet::with_capacity(self.transitions.len());
        let transitions_are_valid = self.transitions.iter().all(|transition| {
            transition.id.0 != 0
                && transition_ids.insert(transition.id)
                && cuts.insert((
                    transition.track_id,
                    transition.left_clip,
                    transition.right_clip,
                ))
                && self.transition_is_valid(transition)
        });
        let mut audio_transition_ids = HashSet::with_capacity(self.audio_transitions.len());
        let mut audio_cuts = HashSet::with_capacity(self.audio_transitions.len());
        let audio_transitions_are_valid = self.audio_transitions.iter().all(|transition| {
            transition.id.0 != 0
                && audio_transition_ids.insert(transition.id)
                && audio_cuts.insert((
                    transition.track_id,
                    transition.left_clip,
                    transition.right_clip,
                ))
                && self.audio_transition_is_valid(transition)
        });
        if self.tracks.iter().all(Self::track_is_valid)
            && titles_are_valid
            && transitions_are_valid
            && audio_transitions_are_valid
        {
            Ok(())
        } else {
            Err(TimelineError::InvariantViolation)
        }
    }

    fn atomic_edit<T>(
        &mut self,
        edit: impl FnOnce(&mut Self) -> Result<T, TimelineError>,
    ) -> Result<T, TimelineError> {
        let previous_generation = self.generation;
        let previous_tracks = &self.tracks;
        let mut staged = self.clone();
        let result = edit(&mut staged)?;
        staged.prune_invalid_transitions();
        staged.check_invariants()?;
        staged.generation = previous_generation;
        staged.structural_generation = self.structural_generation;
        if staged.tracks != *previous_tracks {
            staged.bump_generations(!same_track_layout(previous_tracks, &staged.tracks));
        }
        *self = staged;
        Ok(result)
    }

    /// Preflights ordinary non-ripple edits against immediate neighbors only.
    /// Since a track is ordered/non-overlapping, crossing any distant clip
    /// necessarily crosses one of these neighbors first.
    fn validate_timing_changes(&self, changes: &[TimingChange]) -> Result<(), TimelineError> {
        for change in changes {
            if change.start.0 < 0 {
                return Err(TimelineError::NegativeStart { clip: change.id });
            }
            if change.duration.0 <= 0 {
                return Err(TimelineError::InvalidDuration);
            }
            if change.source_in.0 < 0 {
                return Err(TimelineError::NegativeSourceIn { clip: change.id });
            }
            let location = self
                .clip_location(change.id)
                .ok_or(TimelineError::UnknownClip(change.id))?;
            let original = &self.tracks[location.track_index].clips[location.clip_index];
            let end = checked_add(change.start, change.duration)?;
            let track = &self.tracks[location.track_index];
            let changed_value = |id: ClipId| {
                changes
                    .iter()
                    .find(|candidate| candidate.id == id)
                    .map(|candidate| (candidate.start, candidate.duration))
            };
            let overlaps = |other: &Clip| {
                let (other_start, other_duration) =
                    changed_value(other.id).unwrap_or((other.start, other.duration));
                change.start < checked_add(other_start, other_duration).expect("preflighted tick")
                    && end > other_start
            };
            if location.clip_index > 0 && overlaps(&track.clips[location.clip_index - 1]) {
                return Err(TimelineError::Overlap {
                    track: original.track_id,
                    clip: change.id,
                });
            }
            if let Some(next) = track.clips.get(location.clip_index + 1)
                && overlaps(next)
            {
                return Err(TimelineError::Overlap {
                    track: original.track_id,
                    clip: change.id,
                });
            }
        }
        Ok(())
    }

    fn apply_timing_changes(&mut self, changes: &[TimingChange]) {
        let mut reordered_tracks = Vec::new();
        for change in changes {
            let location = self
                .clip_location(change.id)
                .expect("preflighted clip exists");
            let track = &self.tracks[location.track_index];
            let changed_start = |id: ClipId, fallback: Tick| {
                changes
                    .iter()
                    .find(|candidate| candidate.id == id)
                    .map_or(fallback, |candidate| candidate.start)
            };
            let crossed_previous = location.clip_index > 0
                && changed_start(
                    track.clips[location.clip_index - 1].id,
                    track.clips[location.clip_index - 1].start,
                ) > change.start;
            let crossed_next = track
                .clips
                .get(location.clip_index + 1)
                .is_some_and(|next| changed_start(next.id, next.start) < change.start);
            if (crossed_previous || crossed_next)
                && !reordered_tracks.contains(&location.track_index)
            {
                reordered_tracks.push(location.track_index);
            }
        }
        for change in changes {
            let location = self
                .clip_location(change.id)
                .expect("preflighted clip exists");
            let clip = &mut self.tracks[location.track_index].clips[location.clip_index];
            clip.start = change.start;
            clip.duration = change.duration;
            clip.source_in = change.source_in;
            Self::clamp_fades_to_clip(clip);
        }
        let did_reorder = !reordered_tracks.is_empty();
        for track_index in reordered_tracks {
            self.tracks[track_index]
                .clips
                .sort_by_key(|clip| clip.start);
        }
        if did_reorder {
            self.rebuild_clip_locations();
        }
        self.prune_invalid_transitions();
        debug_assert!(self.check_invariants().is_ok());
    }

    fn trim_ripple_shift(
        clip: &Clip,
        selected: &[Clip],
        shift: Tick,
    ) -> Result<Tick, TimelineError> {
        let mut total = Tick(0);
        for original in selected {
            if clip.id != original.id
                && clip.track_id == original.track_id
                && clip.start >= original.end()
            {
                total = checked_add(total, shift)?;
            }
        }
        Ok(total)
    }

    fn validate_trim_result(
        &self,
        selected: &[Clip],
        selected_changes: &[TimingChange],
        shift: Option<Tick>,
    ) -> Result<(), TimelineError> {
        for track in &self.tracks {
            let mut previous_end = None;
            for clip in &track.clips {
                let (start, duration, source_in) = if let Some(change) =
                    selected_changes.iter().find(|change| change.id == clip.id)
                {
                    (change.start, change.duration, change.source_in)
                } else if let Some(shift) = shift {
                    let total_shift = Self::trim_ripple_shift(clip, selected, shift)?;
                    (
                        checked_add(clip.start, total_shift)?,
                        clip.duration,
                        clip.source_in,
                    )
                } else {
                    (clip.start, clip.duration, clip.source_in)
                };
                if start.0 < 0 {
                    return Err(TimelineError::NegativeStart { clip: clip.id });
                }
                if duration.0 <= 0 {
                    return Err(TimelineError::InvalidDuration);
                }
                if source_in.0 < 0 {
                    return Err(TimelineError::NegativeSourceIn { clip: clip.id });
                }
                let end = checked_add(start, duration)?;
                if previous_end.is_some_and(|previous_end| previous_end > start) {
                    return Err(TimelineError::Overlap {
                        track: track.id,
                        clip: clip.id,
                    });
                }
                previous_end = Some(end);
            }
        }
        Ok(())
    }

    fn selected_clips(
        &self,
        clip_id: ClipId,
        linked_selection: bool,
    ) -> Result<Vec<Clip>, TimelineError> {
        let target = self
            .clip(clip_id)
            .cloned()
            .ok_or(TimelineError::UnknownClip(clip_id))?;
        let mut selected = Vec::with_capacity(if linked_selection {
            self.tracks.len()
        } else {
            1
        });
        selected.push(target.clone());
        let Some(link_id) = linked_selection.then_some(target.link_id).flatten() else {
            return Ok(selected);
        };

        // A track cannot contain two non-overlapping clips at the same start,
        // so binary-search the one candidate on each track instead of walking
        // every clip in the project.
        for track in &self.tracks {
            let index = track
                .clips
                .partition_point(|clip| clip.start < target.start);
            let Some(candidate) = track.clips.get(index) else {
                continue;
            };
            if candidate.id != target.id
                && candidate.link_id == Some(link_id)
                && candidate.start == target.start
                && candidate.duration == target.duration
            {
                selected.push(candidate.clone());
            }
        }
        Ok(selected)
    }

    fn trim(
        &mut self,
        clip_id: ClipId,
        delta: Tick,
        linked_selection: bool,
        ripple: bool,
        edge: FadeEdge,
    ) -> Result<(), TimelineError> {
        let selected = self.selected_clips(clip_id, linked_selection)?;
        let shift = if ripple {
            Some(match edge {
                FadeEdge::In => checked_neg(delta)?,
                FadeEdge::Out => delta,
            })
        } else {
            None
        };
        let mut selected_changes = Vec::with_capacity(selected.len());
        for original in &selected {
            let (start, duration, source_in) = match edge {
                FadeEdge::In => {
                    let duration = checked_add(original.duration, checked_neg(delta)?)?;
                    if duration.0 <= 0 {
                        return Err(TimelineError::InvalidDuration);
                    }
                    let source_in = checked_add(original.source_in, delta)?;
                    if source_in.0 < 0 {
                        return Err(TimelineError::NegativeSourceIn { clip: original.id });
                    }
                    let start = if ripple {
                        original.start
                    } else {
                        let start = checked_add(original.start, delta)?;
                        if start.0 < 0 {
                            return Err(TimelineError::NegativeStart { clip: original.id });
                        }
                        start
                    };
                    (start, duration, source_in)
                }
                FadeEdge::Out => {
                    let duration = checked_add(original.duration, delta)?;
                    if duration.0 <= 0 {
                        return Err(TimelineError::InvalidDuration);
                    }
                    (original.start, duration, original.source_in)
                }
            };
            selected_changes.push(TimingChange {
                id: original.id,
                start,
                duration,
                source_in,
            });
        }

        if shift.is_none() {
            self.validate_timing_changes(&selected_changes)?;
        } else {
            // Ripple deliberately touches a suffix and remains an explicit bulk operation.
            self.validate_trim_result(&selected, &selected_changes, shift)?;
        }
        if selected_changes.iter().all(|change| {
            self.clip(change.id).is_some_and(|clip| {
                clip.start == change.start
                    && clip.duration == change.duration
                    && clip.source_in == change.source_in
            })
        }) && shift.is_none_or(|shift| shift.0 == 0)
        {
            return Ok(());
        }

        if shift.is_none() {
            self.apply_timing_changes(&selected_changes);
            self.bump_generations(true);
            return Ok(());
        }

        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if let Some(change) = selected_changes.iter().find(|change| change.id == clip.id) {
                    clip.start = change.start;
                    clip.duration = change.duration;
                    clip.source_in = change.source_in;
                    Self::clamp_fades_to_clip(clip);
                } else if let Some(shift) = shift {
                    let total_shift = Self::trim_ripple_shift(clip, &selected, shift)?;
                    if total_shift.0 != 0 {
                        clip.start = checked_add(clip.start, total_shift)?;
                    }
                }
            }
        }
        self.bump_generations(true);
        Ok(())
    }

    fn insert_edit_in_place(
        &mut self,
        target: EditTarget,
        media: MediaId,
        at: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<Vec<ClipId>, TimelineError> {
        let tracks = self.edit_tracks(target)?;
        self.validate_edit_interval(at, duration, source_in)?;
        for track in &tracks {
            self.razor(*track, at)?;
            self.shift_later_clips(*track, at, duration, &[])?;
        }
        let inserted = self.insert_on_edit_tracks(target, media, at, duration, source_in)?;
        self.sort_and_validate(&[])?;
        Ok(inserted)
    }

    fn overwrite_edit_in_place(
        &mut self,
        target: EditTarget,
        media: MediaId,
        at: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<Vec<ClipId>, TimelineError> {
        let tracks = self.edit_tracks(target)?;
        self.validate_edit_interval(at, duration, source_in)?;
        let end = checked_add(at, duration)?;
        for track in tracks {
            self.overwrite_track(track, at, end)?;
        }
        let inserted = self.insert_on_edit_tracks(target, media, at, duration, source_in)?;
        self.sort_and_validate(&[])?;
        Ok(inserted)
    }

    fn edit_tracks(&self, target: EditTarget) -> Result<Vec<TrackId>, TimelineError> {
        match target {
            EditTarget::VideoOnly => Ok(vec![self.first_track_of_kind(TrackKind::Video)?]),
            EditTarget::AudioOnly => Ok(vec![self.first_track_of_kind(TrackKind::Audio)?]),
            EditTarget::VideoAndAudio => Ok(vec![
                self.first_track_of_kind(TrackKind::Video)?,
                self.first_track_of_kind(TrackKind::Audio)?,
            ]),
        }
    }

    fn insert_on_edit_tracks(
        &mut self,
        target: EditTarget,
        media: MediaId,
        at: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<Vec<ClipId>, TimelineError> {
        match target {
            EditTarget::VideoOnly => Ok(vec![self.insert_clip(
                self.first_track_of_kind(TrackKind::Video)?,
                media,
                at,
                duration,
                source_in,
            )?]),
            EditTarget::AudioOnly => Ok(vec![self.insert_clip(
                self.first_track_of_kind(TrackKind::Audio)?,
                media,
                at,
                duration,
                source_in,
            )?]),
            EditTarget::VideoAndAudio => {
                let pair = self.insert_linked_av_pair(media, at, duration, source_in)?;
                Ok(vec![pair.video, pair.audio])
            }
        }
    }

    fn validate_edit_interval(
        &self,
        at: Tick,
        duration: Tick,
        source_in: Tick,
    ) -> Result<(), TimelineError> {
        if duration.0 <= 0 {
            return Err(TimelineError::InvalidDuration);
        }
        if at.0 < 0 {
            return Err(TimelineError::NegativeStart { clip: ClipId(0) });
        }
        if source_in.0 < 0 {
            return Err(TimelineError::NegativeSourceIn { clip: ClipId(0) });
        }
        checked_add(at, duration)?;
        Ok(())
    }

    fn shift_later_clips(
        &mut self,
        track_id: TrackId,
        at_or_after: Tick,
        delta: Tick,
        exclude: &[ClipId],
    ) -> Result<(), TimelineError> {
        let track_index = self.track_index(track_id)?;
        for clip in &mut self.tracks[track_index].clips {
            if clip.start >= at_or_after && !exclude.contains(&clip.id) {
                let start = checked_add(clip.start, delta)?;
                if start.0 < 0 {
                    return Err(TimelineError::NegativeStart { clip: clip.id });
                }
                clip.start = start;
            }
        }
        Ok(())
    }

    fn overwrite_track(
        &mut self,
        track_id: TrackId,
        at: Tick,
        end: Tick,
    ) -> Result<(), TimelineError> {
        let track_index = self.track_index(track_id)?;
        let originals = std::mem::take(&mut self.tracks[track_index].clips);
        let mut replacements = Vec::with_capacity(originals.len() + 1);
        for original in originals {
            if original.end() <= at || original.start >= end {
                replacements.push(original);
                continue;
            }

            let keeps_left = original.start < at;
            if keeps_left {
                let mut left = original.clone();
                left.duration = Tick(at.0 - original.start.0);
                left.fade_out = Fade::default();
                Self::clamp_fades_to_clip(&mut left);
                replacements.push(left);
            }
            if original.end() > end {
                let mut right = original.clone();
                right.id = if keeps_left {
                    self.allocate_clip_id()
                } else {
                    original.id
                };
                right.start = end;
                right.duration = Tick(original.end().0 - end.0);
                right.source_in = checked_add(original.source_in, Tick(end.0 - original.start.0))?;
                right.fade_in = Fade::default();
                Self::clamp_fades_to_clip(&mut right);
                replacements.push(right);
            }
        }
        self.tracks[track_index].clips = replacements;
        self.rebuild_clip_locations();
        Ok(())
    }

    fn sort_and_validate(&mut self, _changed: &[ClipId]) -> Result<(), TimelineError> {
        for track in &mut self.tracks {
            track.clips.sort_by_key(|clip| clip.start);
        }
        self.rebuild_clip_locations();
        self.prune_invalid_transitions();
        self.check_invariants()
    }

    fn insert_clip_with_link(
        &mut self,
        track_id: TrackId,
        media: MediaId,
        start: Tick,
        duration: Tick,
        source_in: Tick,
        link_id: Option<LinkId>,
    ) -> Result<ClipId, TimelineError> {
        if duration.0 <= 0 {
            return Err(TimelineError::InvalidDuration);
        }
        let end = checked_add(start, duration)?;
        let id = self.allocate_clip_id();
        let clip = Clip {
            id,
            media,
            track_id,
            link_id,
            start,
            duration,
            source_in,
            enabled: true,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new(),
            transform: ClipTransform::default(),
            fade_in: Fade::default(),
            fade_out: Fade::default(),
        };
        let track_index = self.track_index(track_id)?;
        let track = &mut self.tracks[track_index];
        let insert_at = track
            .clips
            .partition_point(|existing| existing.start < start);
        if insert_at > 0 && track.clips[insert_at - 1].end() > start {
            return Err(TimelineError::Overlap {
                track: track_id,
                clip: id,
            });
        }
        if track
            .clips
            .get(insert_at)
            .is_some_and(|next| end > next.start)
        {
            return Err(TimelineError::Overlap {
                track: track_id,
                clip: id,
            });
        }
        track.clips.insert(insert_at, clip);
        debug_assert!(Self::track_is_valid(track));
        self.rebuild_clip_locations();
        Ok(id)
    }

    fn first_track_of_kind(&self, kind: TrackKind) -> Result<TrackId, TimelineError> {
        self.tracks
            .iter()
            .find(|track| track.kind == kind)
            .map(|track| track.id)
            .ok_or(TimelineError::NoTrackOfKind(kind))
    }

    fn track_index(&self, track_id: TrackId) -> Result<usize, TimelineError> {
        self.tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(TimelineError::UnknownTrack(track_id))
    }

    fn audio_track_mut(&mut self, track_id: TrackId) -> Result<&mut Track, TimelineError> {
        let track = self
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .ok_or(TimelineError::UnknownTrack(track_id))?;
        if track.kind != TrackKind::Audio {
            return Err(TimelineError::AudioTrackOnly(track_id));
        }
        Ok(track)
    }

    fn clip_location(&self, clip_id: ClipId) -> Option<ClipLocation> {
        let location = *self.clip_locations.get(&clip_id)?;
        self.tracks
            .get(location.track_index)
            .and_then(|track| track.clips.get(location.clip_index))
            .filter(|clip| clip.id == clip_id)
            .map(|_| location)
    }

    fn rebuild_clip_locations(&mut self) {
        self.clip_locations.clear();
        let total: usize = self.tracks.iter().map(|track| track.clips.len()).sum();
        self.clip_locations
            .reserve(total.saturating_sub(self.clip_locations.capacity()));
        for (track_index, track) in self.tracks.iter().enumerate() {
            for (clip_index, clip) in track.clips.iter().enumerate() {
                self.clip_locations.insert(
                    clip.id,
                    ClipLocation {
                        track_index,
                        clip_index,
                    },
                );
            }
        }
    }

    fn clip_mut(&mut self, clip_id: ClipId) -> Result<(TrackKind, &mut Clip), TimelineError> {
        if self.clip_location(clip_id).is_none() {
            self.rebuild_clip_locations();
        }
        if let Some(location) = self.clip_location(clip_id) {
            let track = &mut self.tracks[location.track_index];
            return Ok((track.kind, &mut track.clips[location.clip_index]));
        }
        Err(TimelineError::UnknownClip(clip_id))
    }

    fn remove_clip(&mut self, clip_id: ClipId) {
        if self.clip_location(clip_id).is_none() {
            self.rebuild_clip_locations();
        }
        if let Some(location) = self.clip_location(clip_id) {
            self.tracks[location.track_index]
                .clips
                .remove(location.clip_index);
            self.rebuild_clip_locations();
        }
    }

    fn validate_transition(
        &self,
        transition: &VideoTransition,
        replacing: Option<TransitionId>,
    ) -> Result<(), TimelineError> {
        if transition.id.0 == 0
            || transition.duration.0 <= 0
            || !transition.curve.is_finite()
            || self.transitions.iter().any(|item| {
                item.id != replacing.unwrap_or(TransitionId(0))
                    && item.track_id == transition.track_id
                    && item.left_clip == transition.left_clip
                    && item.right_clip == transition.right_clip
            })
        {
            return Err(TimelineError::InvalidTransition);
        }
        let track = self
            .track(transition.track_id)
            .ok_or(TimelineError::UnknownTrack(transition.track_id))?;
        if track.kind != TrackKind::Video {
            return Err(TimelineError::InvalidTransition);
        }
        let left = self
            .clip(transition.left_clip)
            .ok_or(TimelineError::UnknownClip(transition.left_clip))?;
        let right = self
            .clip(transition.right_clip)
            .ok_or(TimelineError::UnknownClip(transition.right_clip))?;
        if left.track_id != transition.track_id
            || right.track_id != transition.track_id
            || left.start >= right.start
            || left.end() != right.start
        {
            return Err(TimelineError::InvalidTransition);
        }
        let left_half = transition.duration.0 / 2;
        let right_half = transition.duration.0 - left_half;
        if left.duration.0 < left_half || right.duration.0 < right_half {
            return Err(TimelineError::InvalidTransition);
        }
        let replaced = replacing.unwrap_or(TransitionId(0));
        let incoming_overlap = self
            .transitions
            .iter()
            .filter(|item| item.id != replaced && item.right_clip == left.id)
            .map(|item| item.duration.0 - item.duration.0 / 2)
            .sum::<i64>();
        let outgoing_overlap = self
            .transitions
            .iter()
            .filter(|item| item.id != replaced && item.left_clip == right.id)
            .map(|item| item.duration.0 / 2)
            .sum::<i64>();
        if incoming_overlap.saturating_add(left_half) > left.duration.0
            || right_half.saturating_add(outgoing_overlap) > right.duration.0
        {
            return Err(TimelineError::InvalidTransition);
        }
        Ok(())
    }

    fn transition_is_valid(&self, transition: &VideoTransition) -> bool {
        self.validate_transition(transition, Some(transition.id))
            .is_ok()
    }

    fn validate_audio_transition(
        &self,
        transition: &AudioTransition,
        replacing: Option<AudioTransitionId>,
    ) -> Result<(), TimelineError> {
        if transition.id.0 == 0
            || transition.duration.0 <= 0
            || self.audio_transitions.iter().any(|item| {
                item.id != replacing.unwrap_or(AudioTransitionId(0))
                    && item.track_id == transition.track_id
                    && item.left_clip == transition.left_clip
                    && item.right_clip == transition.right_clip
            })
        {
            return Err(TimelineError::InvalidTransition);
        }
        let track = self
            .track(transition.track_id)
            .ok_or(TimelineError::UnknownTrack(transition.track_id))?;
        if track.kind != TrackKind::Audio {
            return Err(TimelineError::InvalidTransition);
        }
        let left = self
            .clip(transition.left_clip)
            .ok_or(TimelineError::UnknownClip(transition.left_clip))?;
        let right = self
            .clip(transition.right_clip)
            .ok_or(TimelineError::UnknownClip(transition.right_clip))?;
        if left.track_id != transition.track_id
            || right.track_id != transition.track_id
            || left.start >= right.start
            || left.end() != right.start
        {
            return Err(TimelineError::InvalidTransition);
        }
        let left_half = transition.duration.0 / 2;
        let right_half = transition.duration.0 - left_half;
        if left.duration.0 < left_half || right.duration.0 < right_half {
            return Err(TimelineError::InvalidTransition);
        }
        let replaced = replacing.unwrap_or(AudioTransitionId(0));
        let incoming_overlap = self
            .audio_transitions
            .iter()
            .filter(|item| item.id != replaced && item.right_clip == left.id)
            .map(|item| item.duration.0 - item.duration.0 / 2)
            .sum::<i64>();
        let outgoing_overlap = self
            .audio_transitions
            .iter()
            .filter(|item| item.id != replaced && item.left_clip == right.id)
            .map(|item| item.duration.0 / 2)
            .sum::<i64>();
        if incoming_overlap.saturating_add(left_half) > left.duration.0
            || right_half.saturating_add(outgoing_overlap) > right.duration.0
        {
            return Err(TimelineError::InvalidTransition);
        }
        Ok(())
    }

    fn audio_transition_is_valid(&self, transition: &AudioTransition) -> bool {
        self.validate_audio_transition(transition, Some(transition.id))
            .is_ok()
    }

    fn prune_invalid_transitions(&mut self) {
        // Validate against the full candidate set so two individually valid transitions cannot
        // survive after a structural edit makes their windows overlap inside a shared clip.
        // Removing the first invalid operation and re-evaluating deterministically preserves the
        // newest still-valid neighbor without cloning the transition collection.
        while let Some(index) = self
            .transitions
            .iter()
            .position(|transition| !self.transition_is_valid(transition))
        {
            self.transitions.remove(index);
        }
        while let Some(index) = self
            .audio_transitions
            .iter()
            .position(|transition| !self.audio_transition_is_valid(transition))
        {
            self.audio_transitions.remove(index);
        }
    }

    fn allocate_clip_id(&mut self) -> ClipId {
        let id = ClipId(self.next_clip_id);
        self.next_clip_id = self.next_clip_id.saturating_add(1);
        id
    }

    fn bump_generations(&mut self, structural: bool) {
        if structural {
            self.prune_invalid_transitions();
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        if structural {
            self.structural_generation = self.structural_generation.wrapping_add(1).max(1);
        }
    }

    fn allocate_link_id(&mut self) -> LinkId {
        let id = LinkId(self.next_link_id);
        self.next_link_id = self.next_link_id.saturating_add(1);
        id
    }

    fn clamp_fades_to_clip(clip: &mut Clip) {
        clip.fade_in.duration = Tick(clip.fade_in.duration.0.clamp(0, clip.duration.0));
        clip.fade_out.duration = Tick(
            clip.fade_out
                .duration
                .0
                .clamp(0, (clip.duration.0 - clip.fade_in.duration.0).max(0)),
        );
    }

    fn track_is_valid(track: &Track) -> bool {
        track
            .clips
            .iter()
            .all(|clip| clip.track_id == track.id && clip.duration.0 > 0)
            && track
                .clips
                .windows(2)
                .all(|pair| pair[0].start <= pair[1].start && pair[0].end() <= pair[1].start)
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new_default()
    }
}

/// A compact, derived view of one track for the timeline canvas.
///
/// The authoritative [`Track`] deliberately remains ergonomic for editing.
/// This cache separates the hot drawing/query columns so callers can binary
/// search start/end values without walking `Clip` objects or allocating a
/// widget per clip. Rebuild it only when the owning timeline generation changes.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackCache {
    id: TrackId,
    kind: TrackKind,
    starts: Vec<Tick>,
    ends: Vec<Tick>,
    clip_ids: Vec<ClipId>,
    media_ids: Vec<MediaId>,
}

impl TrackCache {
    fn empty(track: &Track) -> Self {
        Self {
            id: track.id,
            kind: track.kind,
            starts: Vec::new(),
            ends: Vec::new(),
            clip_ids: Vec::new(),
            media_ids: Vec::new(),
        }
    }

    fn rebuild_from_track(&mut self, track: &Track) {
        self.id = track.id;
        self.kind = track.kind;
        self.starts.clear();
        self.ends.clear();
        self.clip_ids.clear();
        self.media_ids.clear();
        self.starts
            .reserve(track.clips.len().saturating_sub(self.starts.capacity()));
        self.ends
            .reserve(track.clips.len().saturating_sub(self.ends.capacity()));
        self.clip_ids
            .reserve(track.clips.len().saturating_sub(self.clip_ids.capacity()));
        self.media_ids
            .reserve(track.clips.len().saturating_sub(self.media_ids.capacity()));

        for clip in &track.clips {
            self.starts.push(clip.start);
            self.ends.push(clip.end());
            self.clip_ids.push(clip.id);
            self.media_ids.push(clip.media);
        }
    }

    pub fn id(&self) -> TrackId {
        self.id
    }

    pub fn kind(&self) -> TrackKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn starts(&self) -> &[Tick] {
        &self.starts
    }

    pub fn ends(&self) -> &[Tick] {
        &self.ends
    }

    /// Returns the contiguous clip index range overlapping `[start, end)`.
    /// Empty/inverted windows return an empty range. The non-overlap invariant
    /// makes both bounds binary searches.
    pub fn visible_range(&self, start: Tick, end: Tick) -> std::ops::Range<usize> {
        if end <= start {
            return 0..0;
        }
        let first = self.ends.partition_point(|clip_end| *clip_end <= start);
        let last = self.starts.partition_point(|clip_start| *clip_start < end);
        first.min(last)..last
    }

    pub fn clip(&self, index: usize) -> Option<CachedClip> {
        Some(CachedClip {
            index,
            id: *self.clip_ids.get(index)?,
            media: *self.media_ids.get(index)?,
            start: *self.starts.get(index)?,
            end: *self.ends.get(index)?,
        })
    }

    /// Returns the clip covering `tick` in O(log n), or none for a gap/boundary after a clip.
    pub fn clip_at(&self, tick: Tick) -> Option<CachedClip> {
        let index = self.ends.partition_point(|clip_end| *clip_end <= tick);
        let clip = self.clip(index)?;
        (clip.start <= tick && tick < clip.end).then_some(clip)
    }

    /// Produces draw records for one viewport. Individual clips that would be
    /// narrower than `minimum_clip_width_px` are coalesced by screen pixel into
    /// density bands. That bounds draw work by viewport width when zoomed out.
    /// Clears `out` then writes the visible draw records without allocating
    /// when the caller retains sufficient capacity between frames.
    pub fn write_draw_records(
        &self,
        view_start: Tick,
        view_end: Tick,
        pixels_per_tick: f64,
        minimum_clip_width_px: f64,
        out: &mut Vec<TrackDrawRecord>,
    ) {
        out.clear();
        self.append_draw_records(
            view_start,
            view_end,
            pixels_per_tick,
            minimum_clip_width_px,
            out,
        );
    }

    /// Appends visible draw records to `out`. A canvas can reuse one buffer
    /// across tracks or frames and avoid a per-frame allocation.
    pub fn append_draw_records(
        &self,
        view_start: Tick,
        view_end: Tick,
        pixels_per_tick: f64,
        minimum_clip_width_px: f64,
        out: &mut Vec<TrackDrawRecord>,
    ) {
        let range = self.visible_range(view_start, view_end);
        if range.is_empty() {
            return;
        }

        let scale = pixels_per_tick.max(0.0);
        let minimum_width = minimum_clip_width_px.max(0.0);
        let mut band: Option<PendingBand> = None;

        for index in range {
            let clip = self.clip(index).expect("cache columns have equal length");
            let width = (clip.end.0.saturating_sub(clip.start.0) as f64) * scale;
            if width >= minimum_width || !scale.is_finite() {
                flush_band(out, &mut band);
                out.push(TrackDrawRecord::Clip(clip));
                continue;
            }

            let pixel =
                (((clip.start.0.saturating_sub(view_start.0)) as f64) * scale).floor() as i64;
            match &mut band {
                Some(current) if current.pixel == pixel => current.extend(clip),
                _ => {
                    flush_band(out, &mut band);
                    band = Some(PendingBand::new(pixel, clip));
                }
            }
        }
        flush_band(out, &mut band);
    }

    /// Convenience wrapper for one-off callers. Hot canvas code should retain
    /// a buffer and use [`Self::write_draw_records`] instead.
    pub fn draw_records(
        &self,
        view_start: Tick,
        view_end: Tick,
        pixels_per_tick: f64,
        minimum_clip_width_px: f64,
    ) -> Vec<TrackDrawRecord> {
        let mut records = Vec::new();
        self.write_draw_records(
            view_start,
            view_end,
            pixels_per_tick,
            minimum_clip_width_px,
            &mut records,
        );
        records
    }
}

/// SoA record for an individually drawable clip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachedClip {
    /// Index into the authoritative track's sorted clip array.
    pub index: usize,
    pub id: ClipId,
    pub media: MediaId,
    pub start: Tick,
    pub end: Tick,
}

/// A screen-pixel bucket of short clips. It is intentionally metadata-light:
/// at far zoom the renderer draws density, not clip labels or thumbnails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipBand {
    pub start: Tick,
    pub end: Tick,
    pub clip_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackDrawRecord {
    Clip(CachedClip),
    Band(ClipBand),
}

#[derive(Clone, Copy, Debug)]
struct PendingBand {
    pixel: i64,
    start: Tick,
    end: Tick,
    clip_count: usize,
}

impl PendingBand {
    fn new(pixel: i64, clip: CachedClip) -> Self {
        Self {
            pixel,
            start: clip.start,
            end: clip.end,
            clip_count: 1,
        }
    }

    fn extend(&mut self, clip: CachedClip) {
        self.end = self.end.max(clip.end);
        self.clip_count += 1;
    }
}

fn flush_band(records: &mut Vec<TrackDrawRecord>, band: &mut Option<PendingBand>) {
    if let Some(band) = band.take() {
        records.push(TrackDrawRecord::Band(ClipBand {
            start: band.start,
            end: band.end,
            clip_count: band.clip_count,
        }));
    }
}

/// Derived cache for all timeline tracks. `generation` belongs to the caller's
/// immutable project snapshot/version; edits bump it and the next UI frame
/// rebuilds this cache once, before rendering.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineCache {
    generation: Option<u64>,
    tracks: Vec<TrackCache>,
}

impl TimelineCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    pub fn tracks(&self) -> &[TrackCache] {
        &self.tracks
    }

    pub fn track(&self, id: TrackId) -> Option<&TrackCache> {
        self.tracks.iter().find(|track| track.id == id)
    }

    /// Rebuilds from the timeline's structural generation only when layout
    /// columns changed. Existing per-track column allocations are retained
    /// when the source track identity and order are unchanged.
    /// Returns whether any cache columns were rebuilt.
    pub fn rebuild_if_stale(&mut self, timeline: &Timeline) -> bool {
        if self.generation == Some(timeline.structural_generation()) {
            return false;
        }
        let common = self.tracks.len().min(timeline.tracks.len());
        for index in 0..common {
            if self.tracks[index].id == timeline.tracks[index].id {
                self.tracks[index].rebuild_from_track(&timeline.tracks[index]);
            } else {
                self.tracks[index] = TrackCache::empty(&timeline.tracks[index]);
                self.tracks[index].rebuild_from_track(&timeline.tracks[index]);
            }
        }
        self.tracks.truncate(timeline.tracks.len());
        for track in &timeline.tracks[common..] {
            let mut cache = TrackCache::empty(track);
            cache.rebuild_from_track(track);
            self.tracks.push(cache);
        }
        self.generation = Some(timeline.structural_generation());
        true
    }

    pub fn invalidate(&mut self) {
        self.generation = None;
    }
}

fn next_id(max_id: u32) -> Result<u32, TimelineSnapshotError> {
    max_id
        .checked_add(1)
        .ok_or(TimelineSnapshotError::IdExhausted)
}

fn same_track_layout(left: &[Track], right: &[Track]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left_track, right_track)| {
            left_track.id == right_track.id
                && left_track.kind == right_track.kind
                && left_track.clips.len() == right_track.clips.len()
                && left_track
                    .clips
                    .iter()
                    .zip(&right_track.clips)
                    .all(|(left_clip, right_clip)| {
                        left_clip.id == right_clip.id
                            && left_clip.track_id == right_clip.track_id
                            // TrackCache stores media IDs for drawing/querying;
                            // source replacement must invalidate that column.
                            && left_clip.media == right_clip.media
                            && left_clip.start == right_clip.start
                            && left_clip.duration == right_clip.duration
                    })
        })
}

fn checked_add(lhs: Tick, rhs: Tick) -> Result<Tick, TimelineError> {
    lhs.0
        .checked_add(rhs.0)
        .map(Tick)
        .ok_or(TimelineError::TickOverflow)
}

fn checked_neg(value: Tick) -> Result<Tick, TimelineError> {
    value
        .0
        .checked_neg()
        .map(Tick)
        .ok_or(TimelineError::TickOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_track(timeline: &Timeline, kind: TrackKind) -> TrackId {
        timeline
            .tracks
            .iter()
            .find(|track| track.kind == kind)
            .unwrap()
            .id
    }

    #[test]
    fn default_layout_has_three_video_and_three_audio_tracks() {
        let timeline = Timeline::new_default();
        assert_eq!(timeline.tracks.len(), 6);
        assert_eq!(
            timeline
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Video)
                .count(),
            3
        );
        assert_eq!(
            timeline
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Audio)
                .count(),
            3
        );
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn titles_restore_legacy_and_round_trip_unicode_multiline_exactly() {
        let timeline = Timeline::new_default();
        let mut legacy = serde_json::to_value(timeline.snapshot()).unwrap();
        legacy.as_object_mut().unwrap().remove("titles");
        assert!(
            Timeline::from_snapshot(serde_json::from_value(legacy).unwrap())
                .unwrap()
                .titles()
                .is_empty()
        );

        let mut timeline = Timeline::new_default();
        let id = timeline
            .add_title(Tick(10), Tick(20), "English\n日本語")
            .unwrap();
        let restored = Timeline::from_snapshot(
            serde_json::from_str(&serde_json::to_string(&timeline.snapshot()).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.title(id).unwrap().text, "English\n日本語");
    }

    #[test]
    fn titles_validate_order_and_history_without_invalidating_track_cache() {
        let mut timeline = Timeline::new_default();
        let before = timeline.snapshot();
        let cache_generation = timeline.structural_generation();
        let low = timeline.add_title(Tick(0), Tick(10), "low").unwrap();
        let high = timeline.add_title(Tick(0), Tick(10), "high").unwrap();
        let mut high_title = timeline.title(high).unwrap().clone();
        high_title.z_order = -1;
        timeline.replace_title(high, high_title).unwrap();
        assert_eq!(
            timeline
                .active_titles(Tick(1))
                .iter()
                .map(|title| title.id)
                .collect::<Vec<_>>(),
            vec![high, low]
        );
        assert_eq!(timeline.structural_generation(), cache_generation);
        let after = timeline.snapshot();
        let mut history = UndoStack::default();
        assert!(history.record(&before, &after));
        assert!(history.undo(&mut timeline));
        assert!(timeline.titles().is_empty());
        assert!(history.redo(&mut timeline));
        assert_eq!(timeline.snapshot(), after);

        let mut invalid = after;
        invalid.titles[0].text = " \n".to_owned();
        assert!(matches!(
            Timeline::from_snapshot(invalid),
            Err(TimelineSnapshotError::InvalidTitle { .. })
        ));
    }

    #[test]
    fn title_ids_reseed_after_snapshot_restore() {
        let mut snapshot = Timeline::new_default().snapshot();
        let title = TitleOverlay {
            id: TitleId(42),
            ..TitleOverlay::default()
        };
        snapshot.titles.push(title);
        let mut timeline = Timeline::from_snapshot(snapshot).unwrap();
        assert_eq!(
            timeline.add_title(Tick(0), Tick(1), "next").unwrap(),
            TitleId(43)
        );
    }

    #[test]
    fn title_id_exhaustion_never_creates_a_duplicate() {
        let mut snapshot = Timeline::new_default().snapshot();
        snapshot.titles.push(TitleOverlay {
            id: TitleId(u32::MAX - 1),
            ..TitleOverlay::default()
        });
        let mut timeline = Timeline::from_snapshot(snapshot).unwrap();
        assert_eq!(
            timeline.add_title(Tick(0), Tick(1), "exhausted"),
            Err(TimelineError::IdExhausted)
        );
        assert_eq!(timeline.titles().len(), 1);
        assert_eq!(timeline.titles()[0].id, TitleId(u32::MAX - 1));
    }

    #[test]
    fn track_mute_is_durable_nonstructural_and_backward_compatible() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let generation = timeline.generation();
        let structural = timeline.structural_generation();
        timeline.set_track_muted(track, true).unwrap();
        assert!(timeline.track(track).unwrap().muted);
        assert_eq!(timeline.generation(), generation + 1);
        assert_eq!(timeline.structural_generation(), structural);
        timeline.set_track_muted(track, true).unwrap();
        assert_eq!(timeline.generation(), generation + 1);

        let mut legacy = serde_json::to_value(timeline.snapshot()).unwrap();
        for track in legacy["tracks"].as_array_mut().unwrap() {
            track.as_object_mut().unwrap().remove("muted");
        }
        let snapshot: TimelineSnapshot = serde_json::from_value(legacy).unwrap();
        let restored = Timeline::from_snapshot(snapshot).unwrap();
        assert!(restored.tracks.iter().all(|track| !track.muted));
    }

    #[test]
    fn clip_enable_is_legacy_safe_nonstructural_and_snapshot_durable() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(7), Tick(0), Tick(20), Tick(0))
            .unwrap();
        let generation = timeline.generation();
        let structural = timeline.structural_generation();
        assert_eq!(
            timeline.set_clip_enabled(clip, false, false).unwrap(),
            [clip]
        );
        assert!(!timeline.clip(clip).unwrap().enabled);
        assert_eq!(timeline.generation(), generation + 1);
        assert_eq!(timeline.structural_generation(), structural);
        assert!(
            timeline
                .set_clip_enabled(clip, false, false)
                .unwrap()
                .is_empty()
        );

        let snapshot = timeline.snapshot();
        let restored = Timeline::from_snapshot(snapshot.clone()).unwrap();
        assert_eq!(restored.snapshot(), snapshot);
        assert!(!restored.clip(clip).unwrap().enabled);

        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy["tracks"].as_array_mut().unwrap()[0]["clips"]
            .as_array_mut()
            .unwrap()[0]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
        let restored = Timeline::from_snapshot(serde_json::from_value(legacy).unwrap()).unwrap();
        assert!(restored.clip(clip).unwrap().enabled);
        restored.check_invariants().unwrap();
    }

    #[test]
    fn clip_enable_link_selection_is_atomic_and_history_round_trips() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(7), Tick(100), Tick(50), Tick(12))
            .unwrap();
        let before = timeline.snapshot();
        assert_eq!(
            timeline.set_clip_enabled(pair.video, false, true).unwrap(),
            [pair.video, pair.audio]
        );
        let after = timeline.snapshot();
        let mut history = UndoStack::default();
        assert!(history.record(&before, &after));
        assert!(history.undo(&mut timeline));
        assert!(timeline.clip(pair.video).unwrap().enabled);
        assert!(timeline.clip(pair.audio).unwrap().enabled);
        assert!(history.redo(&mut timeline));
        assert!(!timeline.clip(pair.video).unwrap().enabled);
        assert!(!timeline.clip(pair.audio).unwrap().enabled);

        assert_eq!(
            timeline.set_clip_enabled(pair.video, true, false).unwrap(),
            [pair.video]
        );
        assert!(timeline.clip(pair.video).unwrap().enabled);
        assert!(!timeline.clip(pair.audio).unwrap().enabled);
        let unchanged = timeline.snapshot();
        assert!(matches!(
            timeline.set_clip_enabled(ClipId(u32::MAX), true, true),
            Err(TimelineError::UnknownClip(ClipId(u32::MAX)))
        ));
        assert_eq!(timeline.snapshot(), unchanged);
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn audio_controls_restore_from_legacy_snapshot_defaults() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let clip = timeline
            .insert_clip(track, MediaId(7), Tick(0), Tick(20), Tick(0))
            .unwrap();
        let mut legacy = serde_json::to_value(timeline.snapshot()).unwrap();
        for track in legacy["tracks"].as_array_mut().unwrap() {
            let track = track.as_object_mut().unwrap();
            for key in ["solo", "gain_db", "pan", "effects"] {
                track.remove(key);
            }
            for clip in track["clips"].as_array_mut().unwrap() {
                let clip = clip.as_object_mut().unwrap();
                for key in ["gain_left_db", "gain_right_db", "effects"] {
                    clip.remove(key);
                }
            }
        }

        let snapshot: TimelineSnapshot = serde_json::from_value(legacy).unwrap();
        let restored = Timeline::from_snapshot(snapshot).unwrap();
        let restored_clip = restored.clip(clip).unwrap();
        assert_eq!(restored_clip.gain_left_db, 0.0);
        assert_eq!(restored_clip.gain_right_db, 0.0);
        assert!(restored_clip.effects.is_empty());
        assert!(restored.tracks.iter().all(|track| {
            !track.solo && track.gain_db == 0.0 && track.pan == 0.0 && track.effects.is_empty()
        }));
    }

    #[test]
    fn audio_effect_bypass_preserves_settings_and_rack_limits_are_validated() {
        assert_eq!(AudioEffect::effective_filter_hz(10), 20);
        assert_eq!(AudioEffect::effective_filter_hz(96_000), 20_000);
        let bypassed = AudioEffect::Bypassed(Box::new(AudioEffect::Eq {
            hz: 1_200,
            db: -3.5,
        }));
        assert!(bypassed.is_valid());
        assert!(bypassed.is_bypassed());
        assert!(bypassed.enabled().is_none());
        let restored: AudioEffect =
            serde_json::from_value(serde_json::to_value(&bypassed).unwrap()).unwrap();
        assert_eq!(restored, bypassed);

        let nested = AudioEffect::Bypassed(Box::new(AudioEffect::Bypassed(Box::new(
            AudioEffect::HighPass { hz: 100 },
        ))));
        assert!(!nested.is_valid());

        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let clip = timeline
            .insert_clip(track, MediaId(7), Tick(0), Tick(20), Tick(0))
            .unwrap();
        let full_rack = vec![AudioEffect::HighPass { hz: 100 }; MAX_AUDIO_EFFECTS_PER_SCOPE];
        timeline
            .set_track_audio_effects(track, full_rack.clone())
            .unwrap();
        timeline
            .set_clip_audio_effects(clip, full_rack.clone())
            .unwrap();
        let too_many = vec![AudioEffect::LowPass { hz: 1_000 }; MAX_AUDIO_EFFECTS_PER_SCOPE + 1];
        assert_eq!(
            timeline.set_track_audio_effects(track, too_many.clone()),
            Err(TimelineError::InvalidAudioEffect)
        );
        assert_eq!(
            timeline.set_clip_audio_effects(clip, too_many.clone()),
            Err(TimelineError::InvalidAudioEffect)
        );

        let mut invalid = timeline.snapshot();
        let audio_track = invalid
            .tracks
            .iter_mut()
            .find(|candidate| candidate.id == track)
            .unwrap();
        audio_track.effects = too_many;
        assert!(matches!(
            Timeline::from_snapshot(invalid),
            Err(TimelineSnapshotError::InvalidTrackEffect { track: rejected }) if rejected == track
        ));
    }

    #[test]
    fn audio_effect_and_stereo_controls_survive_razor_and_history() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let clip = timeline
            .insert_clip(track, MediaId(7), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let before = timeline.snapshot();
        let clip_effects = vec![
            AudioEffect::Eq { hz: 1_000, db: 3.0 },
            AudioEffect::StereoWidth { width: 1.25 },
        ];
        let track_effects = vec![AudioEffect::Compressor, AudioEffect::Limiter];
        timeline
            .set_clip_audio_effects(clip, clip_effects.clone())
            .unwrap();
        timeline.set_audio_channel_gain(clip, -3.0, 4.0).unwrap();
        timeline
            .set_track_audio_effects(track, track_effects.clone())
            .unwrap();
        timeline.set_track_audio_gain(track, 6.0).unwrap();
        timeline.set_track_pan(track, -0.25).unwrap();
        timeline.set_track_solo(track, true).unwrap();
        let after_controls = timeline.snapshot();

        let mut history = UndoStack::default();
        assert!(history.record(&before, &after_controls));
        assert!(history.undo(&mut timeline));
        assert_eq!(timeline.snapshot(), before);
        assert!(history.redo(&mut timeline));
        assert_eq!(timeline.snapshot(), after_controls);

        let split = timeline.razor(track, Tick(40)).unwrap().unwrap();
        for id in [split.left, split.right] {
            let section = timeline.clip(id).unwrap();
            assert_eq!(section.effects, clip_effects);
            assert_eq!(section.gain_left_db, -3.0);
            assert_eq!(section.gain_right_db, 4.0);
        }
        let restored_track = timeline.track(track).unwrap();
        assert_eq!(restored_track.effects, track_effects);
        assert_eq!(restored_track.gain_db, 6.0);
        assert_eq!(restored_track.pan, -0.25);
        assert!(restored_track.solo);
    }

    #[test]
    fn audio_controls_clamp_setters_and_reject_invalid_snapshot_values() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let clip = timeline
            .insert_clip(track, MediaId(7), Tick(0), Tick(20), Tick(0))
            .unwrap();
        timeline
            .set_audio_channel_gain(clip, -500.0, 500.0)
            .unwrap();
        timeline.set_track_audio_gain(track, 500.0).unwrap();
        timeline.set_track_pan(track, -5.0).unwrap();
        assert_eq!(timeline.clip(clip).unwrap().gain_left_db, MIN_GAIN_DB);
        assert_eq!(timeline.clip(clip).unwrap().gain_right_db, MAX_GAIN_DB);
        assert_eq!(timeline.track(track).unwrap().gain_db, MAX_GAIN_DB);
        assert_eq!(timeline.track(track).unwrap().pan, MIN_PAN);
        assert!(matches!(
            timeline.set_track_pan(track, f32::NAN),
            Err(TimelineError::NonFiniteAudioControl)
        ));
        assert!(matches!(
            timeline.set_clip_audio_effects(clip, vec![AudioEffect::Pitch { semitones: 99.0 }]),
            Err(TimelineError::InvalidAudioEffect)
        ));

        let mut invalid = timeline.snapshot();
        invalid.tracks[3].effects = vec![AudioEffect::StereoWidth { width: f32::NAN }];
        assert!(matches!(
            Timeline::from_snapshot(invalid),
            Err(TimelineSnapshotError::InvalidTrackEffect { track: rejected }) if rejected == track
        ));
    }

    #[test]
    fn linked_pair_shares_link_but_keeps_independent_envelopes() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(7), Tick(100), Tick(50), Tick(12))
            .unwrap();
        assert_eq!(
            timeline.clip(pair.video).unwrap().link_id,
            Some(pair.link_id)
        );
        assert_eq!(
            timeline.clip(pair.audio).unwrap().link_id,
            Some(pair.link_id)
        );

        timeline
            .set_fade_duration(pair.video, FadeEdge::In, Tick(20))
            .unwrap();
        timeline
            .set_fade_duration(pair.audio, FadeEdge::Out, Tick(15))
            .unwrap();
        assert_eq!(
            timeline.clip(pair.video).unwrap().fade_in.duration,
            Tick(20)
        );
        assert_eq!(
            timeline.clip(pair.video).unwrap().fade_out.duration,
            Tick(0)
        );
        assert_eq!(timeline.clip(pair.audio).unwrap().fade_in.duration, Tick(0));
        assert_eq!(
            timeline.clip(pair.audio).unwrap().fade_out.duration,
            Tick(15)
        );
    }

    #[test]
    fn linked_razor_splits_video_and_audio_together() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(3), Tick(0), Tick(100), Tick(0))
            .unwrap();
        assert_eq!(timeline.clip_count(), 2);
        timeline
            .set_fade_duration(pair.video, FadeEdge::In, Tick(10))
            .unwrap();
        timeline
            .set_fade_duration(pair.audio, FadeEdge::Out, Tick(12))
            .unwrap();

        let video_track = timeline.clip(pair.video).unwrap().track_id;
        let splits = timeline.razor_linked(video_track, Tick(40)).unwrap();
        assert_eq!(splits.len(), 2);
        assert_eq!(timeline.clip_count(), 4);
        assert_eq!(
            timeline
                .tracks
                .iter()
                .map(|track| track.clips.len())
                .sum::<usize>(),
            4
        );
        assert!(
            timeline
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .all(|clip| { clip.duration == Tick(40) || clip.duration == Tick(60) })
        );
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn gain_is_audio_only_and_clamped() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        timeline.set_audio_gain(pair.audio, 900.0).unwrap();
        assert_eq!(timeline.clip(pair.audio).unwrap().gain_db, MAX_GAIN_DB);
        timeline.set_audio_gain(pair.audio, -900.0).unwrap();
        assert_eq!(timeline.clip(pair.audio).unwrap().gain_db, MIN_GAIN_DB);
        assert_eq!(
            timeline.set_audio_gain(pair.video, 1.0),
            Err(TimelineError::AudioOnly(pair.video))
        );
    }

    #[test]
    fn fades_are_clamped_and_independent() {
        let mut timeline = Timeline::new_default();
        let id = timeline
            .insert_clip(
                first_track(&timeline, TrackKind::Video),
                MediaId(1),
                Tick(0),
                Tick(100),
                Tick(0),
            )
            .unwrap();
        timeline
            .set_fade_duration(id, FadeEdge::In, Tick(80))
            .unwrap();
        timeline
            .set_fade_duration(id, FadeEdge::Out, Tick(80))
            .unwrap();
        timeline.set_fade_curve(id, FadeEdge::In, 3.0).unwrap();
        timeline.set_fade_curve(id, FadeEdge::Out, -3.0).unwrap();
        let clip = timeline.clip(id).unwrap();
        assert_eq!(clip.fade_in.duration, Tick(80));
        assert_eq!(clip.fade_out.duration, Tick(20));
        assert_eq!(clip.fade_in.curve, MAX_FADE_CURVE);
        assert_eq!(clip.fade_out.curve, MIN_FADE_CURVE);
    }

    #[test]
    fn razor_preserves_media_and_source_in() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let id = timeline
            .insert_clip(track, MediaId(8), Tick(100), Tick(90), Tick(1_000))
            .unwrap();
        timeline
            .set_fade_duration(id, FadeEdge::In, Tick(20))
            .unwrap();
        timeline
            .set_fade_duration(id, FadeEdge::Out, Tick(30))
            .unwrap();
        let split = timeline.razor(track, Tick(140)).unwrap().unwrap();
        let left = timeline.clip(split.left).unwrap();
        let right = timeline.clip(split.right).unwrap();
        assert_eq!(
            (left.start, left.duration, left.source_in),
            (Tick(100), Tick(40), Tick(1_000))
        );
        assert_eq!(
            (right.start, right.duration, right.source_in),
            (Tick(140), Tick(50), Tick(1_040))
        );
        assert_eq!(left.fade_in.duration, Tick(20));
        assert_eq!(left.fade_out.duration, Tick(0));
        assert_eq!(right.fade_in.duration, Tick(0));
        assert_eq!(right.fade_out.duration, Tick(30));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn sorted_after_insert() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        timeline
            .insert_clip(track, MediaId(1), Tick(100), Tick(10), Tick(0))
            .unwrap();
        timeline
            .insert_clip(track, MediaId(2), Tick(10), Tick(10), Tick(0))
            .unwrap();
        assert_eq!(
            timeline
                .track(track)
                .unwrap()
                .clips
                .iter()
                .map(|clip| clip.start)
                .collect::<Vec<_>>(),
            vec![Tick(10), Tick(100)]
        );
        assert!(matches!(
            timeline.insert_clip(track, MediaId(3), Tick(105), Tick(10), Tick(0)),
            Err(TimelineError::Overlap { .. })
        ));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn moving_linked_pair_moves_matching_counterpart_and_preserves_clip_data() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(9), Tick(100), Tick(40), Tick(7))
            .unwrap();
        let video_track = timeline.clip(pair.video).unwrap().track_id;
        let audio_track = timeline.clip(pair.audio).unwrap().track_id;

        timeline.move_clip(pair.video, Tick(25)).unwrap();

        for id in [pair.video, pair.audio] {
            let clip = timeline.clip(id).unwrap();
            assert_eq!(clip.start, Tick(125));
            assert_eq!(clip.duration, Tick(40));
            assert_eq!(clip.source_in, Tick(7));
            assert_eq!(clip.link_id, Some(pair.link_id));
        }
        assert_eq!(timeline.clip(pair.video).unwrap().track_id, video_track);
        assert_eq!(timeline.clip(pair.audio).unwrap().track_id, audio_track);
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn moving_linked_pair_rejects_overlap_atomically() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(9), Tick(100), Tick(40), Tick(7))
            .unwrap();
        let video_track = timeline.clip(pair.video).unwrap().track_id;
        timeline
            .insert_clip(video_track, MediaId(10), Tick(170), Tick(20), Tick(0))
            .unwrap();
        let before = timeline.clone();

        assert!(matches!(
            timeline.move_clip(pair.video, Tick(50)),
            Err(TimelineError::Overlap { .. })
        ));
        assert_eq!(timeline.tracks, before.tracks);
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn moving_clip_to_negative_start_is_rejected_without_mutation() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(10), Tick(20), Tick(0))
            .unwrap();
        let before = timeline.clone();

        assert_eq!(
            timeline.move_clip(clip, Tick(-11)),
            Err(TimelineError::NegativeStart { clip })
        );
        assert_eq!(timeline.tracks, before.tracks);
    }

    #[test]
    fn moving_unlinked_clip_leaves_other_clips_in_place() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let first = timeline
            .insert_clip(track, MediaId(1), Tick(10), Tick(10), Tick(0))
            .unwrap();
        let second = timeline
            .insert_clip(track, MediaId(2), Tick(40), Tick(10), Tick(0))
            .unwrap();

        timeline.move_clip(second, Tick(20)).unwrap();

        assert_eq!(timeline.clip(first).unwrap().start, Tick(10));
        assert_eq!(timeline.clip(second).unwrap().start, Tick(60));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn moving_split_section_does_not_move_other_section_with_same_link() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(9), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let video_track = timeline.clip(pair.video).unwrap().track_id;
        let splits = timeline.razor_linked(video_track, Tick(40)).unwrap();
        let video_left = splits
            .iter()
            .find(|split| split.left == pair.video)
            .unwrap()
            .left;

        timeline.move_clip(video_left, Tick(100)).unwrap();

        assert_eq!(timeline.clip(video_left).unwrap().start, Tick(100));
        assert_eq!(
            timeline.clip(pair.audio).unwrap().start,
            Tick(100),
            "the matching audio left section follows"
        );
        assert!(
            timeline
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .filter(|clip| clip.id != video_left && clip.id != pair.audio)
                .any(|clip| clip.start == Tick(40))
        );
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn probe_duration_clamps_matching_clips_without_changing_timing() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(55), Tick(20), Tick(100), Tick(30))
            .unwrap();
        timeline
            .set_fade_duration(pair.video, FadeEdge::Out, Tick(90))
            .unwrap();

        assert_eq!(
            timeline
                .clamp_media_duration(MediaId(55), Tick(80))
                .unwrap(),
            2
        );
        for id in [pair.video, pair.audio] {
            let clip = timeline.clip(id).unwrap();
            assert_eq!(
                (clip.start, clip.source_in, clip.duration),
                (Tick(20), Tick(30), Tick(50))
            );
        }
        assert_eq!(
            timeline.clip(pair.video).unwrap().fade_out.duration,
            Tick(50)
        );
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn provisional_probe_extends_linked_pair_and_caps_at_next_clip() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(55), Tick(0), Tick(15), Tick(0))
            .unwrap();
        let video_track = timeline.clip(pair.video).unwrap().track_id;
        let audio_track = timeline.clip(pair.audio).unwrap().track_id;
        let blocker_video = timeline
            .insert_clip(video_track, MediaId(77), Tick(40), Tick(10), Tick(0))
            .unwrap();
        timeline
            .insert_clip(audio_track, MediaId(77), Tick(40), Tick(10), Tick(0))
            .unwrap();
        let generation = timeline.generation();

        assert_eq!(
            timeline
                .reconcile_provisional_media_duration(
                    MediaId(55),
                    &[pair.video, pair.audio],
                    Tick(60),
                )
                .unwrap(),
            2
        );
        assert_eq!(timeline.clip(pair.video).unwrap().duration, Tick(40));
        assert_eq!(timeline.clip(pair.audio).unwrap().duration, Tick(40));
        assert_eq!(timeline.clip(blocker_video).unwrap().start, Tick(40));
        assert_eq!(timeline.generation(), generation + 1);
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn provisional_probe_shortens_for_source_in_and_ignores_unowned_clips() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(55), Tick(20), Tick(15), Tick(7))
            .unwrap();

        assert_eq!(
            timeline
                .reconcile_provisional_media_duration(MediaId(55), &[pair.video], Tick(17))
                .unwrap(),
            1
        );
        assert_eq!(timeline.clip(pair.video).unwrap().duration, Tick(10));
        assert_eq!(timeline.clip(pair.audio).unwrap().duration, Tick(15));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn snapshot_round_trip_restores_state_and_regenerates_ids() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(12), Tick(10), Tick(25), Tick(2))
            .unwrap();
        timeline.set_audio_gain(pair.audio, -12.0).unwrap();
        timeline
            .set_fade_duration(pair.video, FadeEdge::In, Tick(5))
            .unwrap();

        let encoded = serde_json::to_string(&timeline.snapshot()).unwrap();
        let snapshot: TimelineSnapshot = serde_json::from_str(&encoded).unwrap();
        let mut restored = Timeline::from_snapshot(snapshot).unwrap();
        assert_eq!(restored.snapshot(), timeline.snapshot());
        assert_eq!(restored.add_track(TrackKind::Video), TrackId(7));

        let track = first_track(&restored, TrackKind::Video);
        assert_eq!(
            restored
                .insert_clip(track, MediaId(13), Tick(100), Tick(10), Tick(0))
                .unwrap(),
            ClipId(3)
        );
        let next_pair = restored
            .insert_linked_av_pair(MediaId(14), Tick(200), Tick(10), Tick(0))
            .unwrap();
        assert_eq!(next_pair.link_id, LinkId(2));
        restored.check_invariants().unwrap();
    }

    #[test]
    fn snapshot_rejects_corrupt_state_and_normalizes_finite_envelopes() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(10), Tick(20), Tick(0))
            .unwrap();
        let snapshot = timeline.snapshot();

        let mut duplicate_track = snapshot.clone();
        duplicate_track.tracks[1].id = duplicate_track.tracks[0].id;
        assert!(matches!(
            Timeline::from_snapshot(duplicate_track),
            Err(TimelineSnapshotError::DuplicateTrackId(_))
        ));

        let mut duplicate_clip = snapshot.clone();
        let copied_clip = duplicate_clip.tracks[3].clips[0].clone();
        duplicate_clip.tracks[3].clips.push(copied_clip);
        assert!(matches!(
            Timeline::from_snapshot(duplicate_clip),
            Err(TimelineSnapshotError::DuplicateClipId(_))
        ));

        let mut mismatched_owner = snapshot.clone();
        mismatched_owner.tracks[3].clips[0].track_id = TrackId(999);
        assert!(matches!(
            Timeline::from_snapshot(mismatched_owner),
            Err(TimelineSnapshotError::TrackMismatch { .. })
        ));

        let mut overlap = snapshot.clone();
        overlap.tracks[3].clips.push(Clip {
            id: ClipId(2),
            media: MediaId(2),
            track_id: track,
            link_id: None,
            start: Tick(15),
            duration: Tick(10),
            source_in: Tick(0),
            enabled: true,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new(),
            transform: ClipTransform::default(),
            fade_in: Fade::default(),
            fade_out: Fade::default(),
        });
        assert!(matches!(
            Timeline::from_snapshot(overlap),
            Err(TimelineSnapshotError::UnsortedOrOverlapping { .. })
        ));

        let mut non_finite = snapshot.clone();
        non_finite.tracks[3].clips[0].gain_db = f32::NAN;
        assert!(matches!(
            Timeline::from_snapshot(non_finite),
            Err(TimelineSnapshotError::NonFiniteGain { clip: rejected }) if rejected == clip
        ));

        let mut non_finite_transform = snapshot.clone();
        non_finite_transform.tracks[3].clips[0].transform.anchor_x = f32::NAN;
        assert!(matches!(
            Timeline::from_snapshot(non_finite_transform),
            Err(TimelineSnapshotError::NonFiniteTransform { clip: rejected }) if rejected == clip
        ));

        let mut normalized = snapshot;
        let restored_clip = &mut normalized.tracks[3].clips[0];
        restored_clip.gain_db = 1_000.0;
        restored_clip.fade_in = Fade {
            duration: Tick(100),
            curve: 3.0,
        };
        restored_clip.fade_out = Fade {
            duration: Tick(100),
            curve: -3.0,
        };
        restored_clip.transform.rotation_degrees = 721.0;
        restored_clip.transform.crop_left = 1.0;
        restored_clip.transform.crop_right = 1.0;
        let restored = Timeline::from_snapshot(normalized).unwrap();
        let restored_clip = restored.clip(clip).unwrap();
        assert_eq!(restored_clip.gain_db, MAX_GAIN_DB);
        assert_eq!(
            restored_clip.fade_in,
            Fade {
                duration: Tick(20),
                curve: MAX_FADE_CURVE
            }
        );
        assert_eq!(
            restored_clip.fade_out,
            Fade {
                duration: Tick(0),
                curve: MIN_FADE_CURVE
            }
        );
        assert_eq!(restored_clip.transform.rotation_degrees, 1.0);
        assert!(
            restored_clip.transform.crop_left + restored_clip.transform.crop_right
                <= ClipTransform::MAX_CROP_TOTAL
        );
    }

    #[test]
    fn move_with_link_toggle_keeps_counterpart_optional() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(10), Tick(20), Tick(0))
            .unwrap();

        timeline
            .move_clip_with_link(pair.video, Tick(15), false)
            .unwrap();
        assert_eq!(timeline.clip(pair.video).unwrap().start, Tick(25));
        assert_eq!(timeline.clip(pair.audio).unwrap().start, Tick(10));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn trim_and_ripple_keep_source_continuity_and_shift_later_clips() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(10))
            .unwrap();
        let video_track = timeline.clip(pair.video).unwrap().track_id;
        let audio_track = timeline.clip(pair.audio).unwrap().track_id;
        let later_video = timeline
            .insert_clip(video_track, MediaId(2), Tick(100), Tick(20), Tick(0))
            .unwrap();
        let later_audio = timeline
            .insert_clip(audio_track, MediaId(2), Tick(100), Tick(20), Tick(0))
            .unwrap();

        timeline
            .trim_start(pair.video, Tick(20), false, false)
            .unwrap();
        assert_eq!(
            (
                timeline.clip(pair.video).unwrap().start,
                timeline.clip(pair.video).unwrap().duration,
                timeline.clip(pair.video).unwrap().source_in
            ),
            (Tick(20), Tick(80), Tick(30))
        );
        assert_eq!(timeline.clip(pair.audio).unwrap().start, Tick(0));

        timeline
            .trim_end(pair.video, Tick(-20), true, true)
            .unwrap();
        assert_eq!(timeline.clip(pair.video).unwrap().duration, Tick(60));
        // The earlier unlinked trim made these no longer exact matching
        // sections, so the linked toggle intentionally leaves audio alone.
        assert_eq!(timeline.clip(pair.audio).unwrap().duration, Tick(100));
        assert_eq!(timeline.clip(later_video).unwrap().start, Tick(80));
        assert_eq!(timeline.clip(later_audio).unwrap().start, Tick(100));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn slip_changes_source_without_changing_timeline_section() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(20), Tick(30), Tick(5))
            .unwrap();
        timeline.slip_clip(pair.video, Tick(7), true).unwrap();
        for id in [pair.video, pair.audio] {
            let clip = timeline.clip(id).unwrap();
            assert_eq!(
                (clip.start, clip.duration, clip.source_in),
                (Tick(20), Tick(30), Tick(12))
            );
        }
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn insert_edit_splits_and_ripples_default_video_and_audio_tracks() {
        let mut timeline = Timeline::new_default();
        let original = timeline
            .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(10))
            .unwrap();
        let inserted = timeline
            .insert_edit(
                EditTarget::VideoAndAudio,
                MediaId(2),
                Tick(40),
                Tick(10),
                Tick(3),
            )
            .unwrap();
        assert_eq!(inserted.len(), 2);
        let video_track = timeline.clip(original.video).unwrap().track_id;
        let video = &timeline.track(video_track).unwrap().clips;
        assert_eq!(
            video
                .iter()
                .map(|clip| (clip.start, clip.duration))
                .collect::<Vec<_>>(),
            vec![
                (Tick(0), Tick(40)),
                (Tick(40), Tick(10)),
                (Tick(50), Tick(60))
            ]
        );
        assert_eq!(video[2].source_in, Tick(50));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn video_only_insert_and_overwrite_preserve_audio_and_are_atomic() {
        for overwrite in [false, true] {
            let mut timeline = Timeline::new_default();
            let original = timeline
                .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(10))
                .unwrap();
            let audio_track = timeline.clip(original.audio).unwrap().track_id;
            let audio_before = timeline.track(audio_track).unwrap().clips.clone();
            let inserted = if overwrite {
                timeline.overwrite_edit(
                    EditTarget::VideoOnly,
                    MediaId(2),
                    Tick(40),
                    Tick(20),
                    Tick(0),
                )
            } else {
                timeline.insert_edit(
                    EditTarget::VideoOnly,
                    MediaId(2),
                    Tick(40),
                    Tick(20),
                    Tick(0),
                )
            }
            .unwrap();
            assert_eq!(inserted.len(), 1);
            assert_eq!(timeline.track(audio_track).unwrap().clips, audio_before);
            timeline.check_invariants().unwrap();

            let valid = timeline.snapshot();
            let failed = timeline.overwrite_edit(
                EditTarget::VideoOnly,
                MediaId(3),
                Tick(-1),
                Tick(20),
                Tick(0),
            );
            assert!(failed.is_err());
            assert_eq!(timeline.snapshot(), valid);
        }
    }

    #[test]
    fn overwrite_edit_preserves_source_offset_and_outer_fade_tails() {
        let mut timeline = Timeline::new_default();
        let original = timeline
            .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(10))
            .unwrap();
        timeline
            .set_fade_duration(original.video, FadeEdge::In, Tick(5))
            .unwrap();
        timeline
            .set_fade_duration(original.video, FadeEdge::Out, Tick(5))
            .unwrap();

        timeline
            .overwrite_edit(
                EditTarget::VideoAndAudio,
                MediaId(2),
                Tick(40),
                Tick(20),
                Tick(0),
            )
            .unwrap();
        let video_track = timeline.clip(original.video).unwrap().track_id;
        let video = &timeline.track(video_track).unwrap().clips;
        assert_eq!(
            video
                .iter()
                .map(|clip| (clip.media, clip.start, clip.duration, clip.source_in))
                .collect::<Vec<_>>(),
            vec![
                (MediaId(1), Tick(0), Tick(40), Tick(10)),
                (MediaId(2), Tick(40), Tick(20), Tick(0)),
                (MediaId(1), Tick(60), Tick(40), Tick(70))
            ]
        );
        assert_eq!(video[0].fade_in.duration, Tick(5));
        assert_eq!(video[0].fade_out.duration, Tick(0));
        assert_eq!(video[2].fade_in.duration, Tick(0));
        assert_eq!(video[2].fade_out.duration, Tick(5));
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn replace_media_retains_timeline_shape_and_can_follow_link() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(20), Tick(30), Tick(5))
            .unwrap();
        timeline
            .replace_clip_media(pair.video, MediaId(9), Tick(12), true)
            .unwrap();
        for id in [pair.video, pair.audio] {
            let clip = timeline.clip(id).unwrap();
            assert_eq!(
                (clip.media, clip.start, clip.duration, clip.source_in),
                (MediaId(9), Tick(20), Tick(30), Tick(12))
            );
        }
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn roll_edit_moves_shared_boundary_and_exact_linked_counterpart() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let video_track = timeline.clip(pair.video).unwrap().track_id;
        let splits = timeline.razor_linked(video_track, Tick(40)).unwrap();
        let video_left = splits
            .iter()
            .find(|split| split.left == pair.video)
            .unwrap()
            .left;
        let video_right = splits
            .iter()
            .find(|split| split.left == pair.video)
            .unwrap()
            .right;

        timeline
            .roll_edit(video_left, video_right, Tick(10), true)
            .unwrap();
        let video = timeline.track(video_track).unwrap();
        assert_eq!(
            video
                .clips
                .iter()
                .map(|clip| (clip.start, clip.duration, clip.source_in))
                .collect::<Vec<_>>(),
            vec![(Tick(0), Tick(50), Tick(0)), (Tick(50), Tick(50), Tick(50))]
        );
        let audio_track = timeline.clip(pair.audio).unwrap().track_id;
        assert_eq!(
            timeline
                .track(audio_track)
                .unwrap()
                .clips
                .iter()
                .map(|clip| (clip.start, clip.duration, clip.source_in))
                .collect::<Vec<_>>(),
            vec![(Tick(0), Tick(50), Tick(0)), (Tick(50), Tick(50), Tick(50))]
        );
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn roll_edit_rejects_nonadjacent_or_invalid_boundary_without_mutation() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let left = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(20), Tick(0))
            .unwrap();
        let gapped = timeline
            .insert_clip(track, MediaId(2), Tick(30), Tick(20), Tick(0))
            .unwrap();
        let before = timeline.clone();
        assert!(matches!(
            timeline.roll_edit(left, gapped, Tick(1), false),
            Err(TimelineError::RollNotAdjacent { .. })
        ));
        assert_eq!(timeline.tracks, before.tracks);

        let mut adjacent = Timeline::new_default();
        let track = first_track(&adjacent, TrackKind::Video);
        let left = adjacent
            .insert_clip(track, MediaId(1), Tick(0), Tick(20), Tick(0))
            .unwrap();
        let right = adjacent
            .insert_clip(track, MediaId(2), Tick(20), Tick(20), Tick(0))
            .unwrap();
        let before = adjacent.clone();
        assert_eq!(
            adjacent.roll_edit(left, right, Tick(20), false),
            Err(TimelineError::InvalidDuration)
        );
        assert_eq!(adjacent.tracks, before.tracks);
    }

    #[test]
    fn failed_insert_overwrite_and_replace_are_atomic() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(10), Tick(30), Tick(0))
            .unwrap();
        let before = timeline.clone();
        assert!(matches!(
            timeline.insert_edit(
                EditTarget::VideoAndAudio,
                MediaId(2),
                Tick(-1),
                Tick(5),
                Tick(0)
            ),
            Err(TimelineError::NegativeStart { .. })
        ));
        assert_eq!(timeline.tracks, before.tracks);

        assert_eq!(
            timeline.overwrite_edit(
                EditTarget::AudioOnly,
                MediaId(2),
                Tick(10),
                Tick(0),
                Tick(0)
            ),
            Err(TimelineError::InvalidDuration)
        );
        assert_eq!(timeline.tracks, before.tracks);

        assert_eq!(
            timeline.replace_clip_media(pair.video, MediaId(3), Tick(-1), true),
            Err(TimelineError::NegativeSourceIn { clip: pair.video })
        );
        assert_eq!(timeline.tracks, before.tracks);
    }

    fn cached_timeline_with_clips(clips: Vec<Clip>) -> (Timeline, TrackId) {
        let track_id = TrackId(1);
        let timeline = Timeline::from_snapshot(TimelineSnapshot {
            tracks: vec![Track {
                id: track_id,
                kind: TrackKind::Video,
                muted: false,
                solo: false,
                gain_db: 0.0,
                pan: 0.0,
                effects: Vec::new(),
                clips,
            }],
            titles: Vec::new(),
            transitions: Vec::new(),
            audio_transitions: Vec::new(),
        })
        .expect("test clips are sorted and non-overlapping");
        (timeline, track_id)
    }

    fn test_clip(id: u32, start: i64, duration: i64) -> Clip {
        Clip {
            id: ClipId(id),
            media: MediaId(id),
            track_id: TrackId(1),
            link_id: None,
            start: Tick(start),
            duration: Tick(duration),
            source_in: Tick(0),
            enabled: true,
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new(),
            transform: ClipTransform::default(),
            fade_in: Fade::default(),
            fade_out: Fade::default(),
        }
    }

    #[test]
    fn clip_transform_defaults_and_clamps() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(10), Tick(0))
            .unwrap();
        assert_eq!(
            timeline.clip(clip).unwrap().transform,
            ClipTransform::default()
        );
        let mut xf = ClipTransform {
            opacity: 0.5,
            scale_x: 1.5,
            pos_x: 0.25,
            flip_h: true,
            ..ClipTransform::default()
        };
        timeline.set_clip_transform(clip, xf).unwrap();
        assert_eq!(timeline.clip(clip).unwrap().transform.opacity, 0.5);
        xf.scale_x = 99.0;
        timeline.set_clip_transform(clip, xf).unwrap();
        assert_eq!(
            timeline.clip(clip).unwrap().transform.scale_x,
            ClipTransform::MAX_SCALE
        );

        xf.rotation_degrees = 540.0;
        xf.anchor_x = -1.0;
        xf.anchor_y = 2.0;
        xf.crop_left = 1.0;
        xf.crop_right = 1.0;
        xf.crop_top = 0.9;
        xf.crop_bottom = 0.9;
        xf.sizing_mode = ClipSizingMode::Fill;
        timeline.set_clip_transform(clip, xf).unwrap();
        let saved = timeline.clip(clip).unwrap().transform;
        assert_eq!(saved.rotation_degrees, -180.0);
        assert_eq!(saved.anchor_x, 0.0);
        assert_eq!(saved.anchor_y, 1.0);
        assert!(saved.crop_left + saved.crop_right <= ClipTransform::MAX_CROP_TOTAL);
        assert!(saved.crop_top + saved.crop_bottom <= ClipTransform::MAX_CROP_TOTAL);
        assert_eq!(saved.sizing_mode, ClipSizingMode::Fill);
    }

    #[test]
    fn legacy_transform_json_defaults_and_noop_do_not_bump_generation() {
        let legacy: ClipTransform = serde_json::from_str(
            r#"{"opacity":0.5,"scale_x":1.25,"scale_y":0.75,"pos_x":0.2,"pos_y":-0.1,"flip_h":true,"flip_v":false}"#,
        )
        .unwrap();
        assert_eq!(legacy.rotation_degrees, 0.0);
        assert_eq!(legacy.anchor_x, 0.5);
        assert_eq!(legacy.anchor_y, 0.5);
        assert_eq!(legacy.sizing_mode, ClipSizingMode::Fit);
        assert_eq!(
            legacy.crop_left + legacy.crop_right + legacy.crop_top + legacy.crop_bottom,
            0.0
        );

        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(10), Tick(0))
            .unwrap();
        let generation = timeline.generation();
        timeline
            .set_clip_transform(clip, ClipTransform::default())
            .unwrap();
        assert_eq!(timeline.generation(), generation);
        let changed = ClipTransform {
            rotation_degrees: 30.0,
            ..ClipTransform::default()
        };
        timeline.set_clip_transform(clip, changed).unwrap();
        assert_eq!(timeline.generation(), generation + 1);
        timeline.set_clip_transform(clip, changed).unwrap();
        assert_eq!(timeline.generation(), generation + 1);
    }

    #[test]
    fn transforms_reject_nonfinite_new_fields() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(10), Tick(0))
            .unwrap();
        let mut transform = ClipTransform {
            crop_bottom: f32::NAN,
            ..ClipTransform::default()
        };
        assert_eq!(
            timeline.set_clip_transform(clip, transform),
            Err(TimelineError::NonFiniteTransform)
        );
        transform.crop_bottom = 0.0;
        transform.rotation_degrees = f32::INFINITY;
        assert_eq!(
            timeline.set_clip_transform(clip, transform),
            Err(TimelineError::NonFiniteTransform)
        );
    }

    #[test]
    fn cache_rebuilds_only_when_timeline_generation_changes() {
        let (mut timeline, track_id) =
            cached_timeline_with_clips(vec![test_clip(1, 10, 5), test_clip(2, 20, 5)]);
        let mut cache = TimelineCache::new();

        assert_eq!(timeline.generation(), 1);
        assert!(cache.rebuild_if_stale(&timeline));
        assert_eq!(cache.generation(), Some(1));
        assert!(!cache.rebuild_if_stale(&timeline));
        timeline.add_track(TrackKind::Audio);
        assert_eq!(timeline.generation(), 2);
        assert!(cache.rebuild_if_stale(&timeline));
        assert_eq!(
            cache.track(track_id).unwrap().starts(),
            &[Tick(10), Tick(20)]
        );

        cache.invalidate();
        assert_eq!(cache.generation(), None);
        assert!(cache.rebuild_if_stale(&timeline));
    }

    #[test]
    fn generation_bumps_once_for_durable_edits_not_failed_or_noop_edits() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let initial = timeline.generation();
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(10), Tick(20), Tick(0))
            .unwrap();
        assert_eq!(timeline.generation(), initial + 1);

        let after_insert = timeline.generation();
        assert!(matches!(
            timeline.insert_clip(track, MediaId(2), Tick(15), Tick(5), Tick(0)),
            Err(TimelineError::Overlap { .. })
        ));
        assert_eq!(timeline.generation(), after_insert);

        timeline.move_clip_with_link(clip, Tick(0), false).unwrap();
        assert_eq!(timeline.generation(), after_insert);
        assert_eq!(timeline.razor(track, Tick(10)).unwrap(), None);
        assert_eq!(timeline.generation(), after_insert);
        timeline.set_audio_gain(clip, 0.0).unwrap();
        assert_eq!(timeline.generation(), after_insert);

        timeline.set_audio_gain(clip, -6.0).unwrap();
        assert_eq!(timeline.generation(), after_insert + 1);
        let after_gain = timeline.generation();
        timeline.razor_linked(track, Tick(20)).unwrap();
        assert_eq!(timeline.generation(), after_gain + 1);
    }

    #[test]
    fn source_and_envelope_edits_do_not_invalidate_structural_cache() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(10), Tick(20), Tick(5))
            .unwrap();
        let mut cache = TimelineCache::new();
        assert!(cache.rebuild_if_stale(&timeline));
        let structural = timeline.structural_generation();

        timeline.set_audio_gain(clip, -6.0).unwrap();
        timeline
            .set_fade_duration(clip, FadeEdge::In, Tick(4))
            .unwrap();
        timeline.set_fade_curve(clip, FadeEdge::In, 0.5).unwrap();
        timeline.slip_clip(clip, Tick(1), false).unwrap();
        assert_eq!(timeline.structural_generation(), structural);
        assert!(!cache.rebuild_if_stale(&timeline));

        timeline.move_clip_with_link(clip, Tick(1), false).unwrap();
        assert_eq!(timeline.structural_generation(), structural + 1);
        assert!(cache.rebuild_if_stale(&timeline));
        timeline.trim_end(clip, Tick(1), false, false).unwrap();
        assert_eq!(timeline.structural_generation(), structural + 2);
        assert!(cache.rebuild_if_stale(&timeline));
    }

    #[test]
    fn structural_cache_rebuild_reuses_track_column_allocations() {
        let (mut timeline, track_id) = cached_timeline_with_clips(
            (1..=100)
                .map(|id| test_clip(id, (id as i64 - 1) * 2, 1))
                .collect(),
        );
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);
        let cached_track = cache.track(track_id).unwrap();
        let start_pointer = cached_track.starts.as_ptr();
        let start_capacity = cached_track.starts.capacity();
        let end_pointer = cached_track.ends.as_ptr();
        let end_capacity = cached_track.ends.capacity();

        timeline
            .move_clip_with_link(ClipId(1), Tick(1), false)
            .unwrap();
        assert!(cache.rebuild_if_stale(&timeline));
        let cached_track = cache.track(track_id).unwrap();
        assert_eq!(cached_track.starts.as_ptr(), start_pointer);
        assert_eq!(cached_track.starts.capacity(), start_capacity);
        assert_eq!(cached_track.ends.as_ptr(), end_pointer);
        assert_eq!(cached_track.ends.capacity(), end_capacity);
    }

    #[test]
    fn replacing_media_invalidates_cached_media_ids() {
        let (mut timeline, track_id) = cached_timeline_with_clips(vec![test_clip(1, 10, 5)]);
        let mut cache = TimelineCache::new();
        assert!(cache.rebuild_if_stale(&timeline));
        assert_eq!(
            cache.track(track_id).unwrap().clip(0).unwrap().media,
            MediaId(1)
        );

        timeline
            .replace_clip_media(ClipId(1), MediaId(99), Tick(0), false)
            .unwrap();
        assert!(cache.rebuild_if_stale(&timeline));
        assert_eq!(
            cache.track(track_id).unwrap().clip(0).unwrap().media,
            MediaId(99)
        );
    }

    #[test]
    fn retained_draw_record_buffer_is_reused() {
        let (timeline, track_id) = cached_timeline_with_clips(
            (1..=100)
                .map(|id| test_clip(id, (id as i64 - 1) * 2, 1))
                .collect(),
        );
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);
        let mut records = Vec::with_capacity(128);
        let capacity = records.capacity();
        cache.track(track_id).unwrap().write_draw_records(
            Tick(0),
            Tick(200),
            0.01,
            2.0,
            &mut records,
        );
        assert_eq!(records.capacity(), capacity);
        assert!(!records.is_empty());
    }

    #[test]
    fn visible_range_matches_brute_force_for_randomized_windows() {
        let mut seed = 0xD1CE_BA5Eu64;
        let mut start = 0_i64;
        let mut clips = Vec::new();
        for id in 1..=1_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            start += 1 + ((seed >> 32) % 11) as i64;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let duration = 1 + ((seed >> 32) % 17) as i64;
            clips.push(test_clip(id, start, duration));
            start += duration;
        }
        let (timeline, track_id) = cached_timeline_with_clips(clips);
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);
        let track = cache.track(track_id).unwrap();

        for _ in 0..2_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let window_start = ((seed >> 24) % (start as u64 + 20)) as i64;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let window_end = window_start + ((seed >> 32) % 80) as i64;
            let expected: Vec<_> = if window_end > window_start {
                timeline.tracks[0]
                    .clips
                    .iter()
                    .enumerate()
                    .filter_map(|(index, clip)| {
                        (clip.end() > Tick(window_start) && clip.start < Tick(window_end))
                            .then_some(index)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let actual: Vec<_> = track
                .visible_range(Tick(window_start), Tick(window_end))
                .collect();
            assert_eq!(actual, expected, "window {window_start}..{window_end}");
        }
    }

    #[test]
    fn clip_at_playhead() {
        let (timeline, track_id) =
            cached_timeline_with_clips(vec![test_clip(1, 10, 5), test_clip(2, 20, 5)]);
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);
        let track = cache.track(track_id).unwrap();
        assert!(track.clip_at(Tick(9)).is_none());
        assert_eq!(track.clip_at(Tick(10)).unwrap().id, ClipId(1));
        assert_eq!(track.clip_at(Tick(14)).unwrap().id, ClipId(1));
        assert!(track.clip_at(Tick(15)).is_none());
        assert_eq!(track.clip_at(Tick(20)).unwrap().id, ClipId(2));
        assert!(track.clip_at(Tick(25)).is_none());
    }

    #[test]
    fn trim_does_not_invert_duration() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(10), Tick(0))
            .unwrap();
        let before = timeline.clip(clip).unwrap().clone();
        assert!(timeline.trim_start(clip, Tick(10), false, false).is_err());
        assert_eq!(timeline.clip(clip), Some(&before));
        assert!(timeline.trim_end(clip, Tick(-10), false, false).is_err());
        assert_eq!(timeline.clip(clip), Some(&before));
    }

    #[test]
    fn banding_reduces_draw_count_when_zoomed_out() {
        let clips = (1..=50_000)
            .map(|id| test_clip(id, (id as i64 - 1) * 2, 1))
            .collect();
        let (timeline, track_id) = cached_timeline_with_clips(clips);
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);

        let records =
            cache
                .track(track_id)
                .unwrap()
                .draw_records(Tick(0), Tick(100_000), 0.01, 2.0);
        let represented: usize = records
            .iter()
            .map(|record| match record {
                TrackDrawRecord::Clip(_) => 1,
                TrackDrawRecord::Band(band) => band.clip_count,
            })
            .sum();
        assert_eq!(represented, 50_000);
        assert!(
            records.len() <= 1_001,
            "zoomed-out 50k clips should be bounded by the 1,000-pixel viewport, got {} records",
            records.len()
        );
    }

    #[test]
    fn hot_move_and_trim_keep_fifty_thousand_clip_storage_in_place() {
        let mut clips: Vec<_> = (1..=50_000)
            .map(|id| test_clip(id, (id as i64 - 1) * 2, 1))
            .collect();
        clips.last_mut().unwrap().source_in = Tick(1);
        let (mut timeline, track_id) = cached_timeline_with_clips(clips);
        let before = timeline.track(track_id).unwrap();
        let pointer = before.clips.as_ptr();
        let capacity = before.clips.capacity();
        let index_capacity = timeline.clip_locations.capacity();

        timeline
            .move_clip_with_link(ClipId(1), Tick(1), false)
            .unwrap();
        let after_move = timeline.track(track_id).unwrap();
        assert_eq!(after_move.clips.as_ptr(), pointer);
        assert_eq!(after_move.clips.capacity(), capacity);
        assert_eq!(timeline.clip_locations.capacity(), index_capacity);

        timeline
            .trim_start(ClipId(50_000), Tick(-1), false, false)
            .unwrap();
        let after_trim = timeline.track(track_id).unwrap();
        assert_eq!(after_trim.clips.as_ptr(), pointer);
        assert_eq!(after_trim.clips.capacity(), capacity);
        assert_eq!(timeline.clip_locations.capacity(), index_capacity);
        timeline.check_invariants().unwrap();
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn release_fifty_thousand_clip_move_and_trim_finish_under_two_ms() {
        use std::time::{Duration, Instant};

        let clips = (1..=50_000)
            .map(|id| test_clip(id, (id as i64 - 1) * 3, 1))
            .collect();
        let (mut timeline, _) = cached_timeline_with_clips(clips);
        let id = ClipId(25_000);

        let move_started = Instant::now();
        timeline.move_clip_with_link(id, Tick(1), false).unwrap();
        assert!(
            move_started.elapsed() < Duration::from_millis(2),
            "50k move took {:?}",
            move_started.elapsed()
        );

        let trim_started = Instant::now();
        timeline.trim_end(id, Tick(1), false, false).unwrap();
        assert!(
            trim_started.elapsed() < Duration::from_millis(2),
            "50k trim took {:?}",
            trim_started.elapsed()
        );
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn fifty_thousand_visible_range_is_sub_millisecond() {
        use std::time::{Duration, Instant};

        let clips = (1..=50_000)
            .map(|id| test_clip(id, (id as i64 - 1) * 3, 1))
            .collect();
        let (timeline, track_id) = cached_timeline_with_clips(clips);
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);
        let track = cache.track(track_id).unwrap();
        let started = Instant::now();
        for offset in 0..10_000 {
            let range = track.visible_range(Tick(75_000 + offset), Tick(75_300 + offset));
            std::hint::black_box(range);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(10),
            "10k binary-search visibility queries took {elapsed:?}"
        );
    }

    #[test]
    fn undo_stack_roundtrip() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(7), Tick(10), Tick(100), Tick(3))
            .unwrap();
        let before = timeline.snapshot();

        timeline
            .move_clip_with_link(pair.video, Tick(20), true)
            .unwrap();
        timeline.set_audio_gain(pair.audio, 6.0).unwrap();
        timeline
            .set_fade_duration(pair.video, FadeEdge::Out, Tick(20))
            .unwrap();
        timeline.razor_linked(TrackId(1), Tick(80)).unwrap();
        let added = timeline.add_track(TrackKind::Audio);
        timeline.set_track_muted(added, true).unwrap();
        let after = timeline.snapshot();

        let mut history = UndoStack::default();
        assert!(history.record(&before, &after));
        assert_eq!(history.len(), 1);
        assert!(history.can_undo());
        assert!(!history.can_redo());

        assert!(history.undo(&mut timeline));
        assert_eq!(timeline.snapshot(), before);
        timeline.check_invariants().unwrap();
        assert!(history.can_redo());

        assert!(history.redo(&mut timeline));
        assert_eq!(timeline.snapshot(), after);
        timeline.check_invariants().unwrap();
    }

    #[test]
    fn same_cardinality_history_records_one_relocated_clip_without_map_diff() {
        let clips = (1..=128)
            .map(|id| test_clip(id, (id as i64 - 1) * 3, 1))
            .collect();
        let (mut timeline, _) = cached_timeline_with_clips(clips);
        let before = timeline.snapshot();
        timeline
            .move_clip_with_link(ClipId(96), Tick(-280), false)
            .unwrap();
        let after = timeline.snapshot();

        let batch = timeline_delta(&before, &after).expect("relocation delta");
        assert_eq!(
            batch.edits,
            vec![TimelineEdit::PatchClip {
                before: before.tracks[0].clips[95].clone(),
                after: timeline.clip(ClipId(96)).unwrap().clone(),
            }]
        );

        let mut history = UndoStack::default();
        assert!(history.record(&before, &after));
        assert!(history.undo(&mut timeline));
        assert_eq!(timeline.snapshot(), before);
        assert!(history.redo(&mut timeline));
        assert_eq!(timeline.snapshot(), after);
    }

    #[test]
    fn undo_stack_is_capped_and_a_new_edit_clears_redo() {
        let mut timeline = Timeline::new_default();
        let mut history = UndoStack::with_capacity(3);
        for _ in 0..5 {
            let before = timeline.snapshot();
            timeline.add_track(TrackKind::Video);
            assert!(history.record(&before, &timeline.snapshot()));
        }
        assert_eq!(history.len(), 3);
        assert!(history.undo(&mut timeline));
        assert!(history.can_redo());

        let before = timeline.snapshot();
        timeline.add_track(TrackKind::Audio);
        assert!(history.record(&before, &timeline.snapshot()));
        assert!(!history.can_redo());
        assert!(!history.record(&timeline.snapshot(), &timeline.snapshot()));
    }

    #[test]
    fn delete_linked_clip_removes_exact_video_and_audio_section() {
        let mut timeline = Timeline::new_default();
        let pair = timeline
            .insert_linked_av_pair(MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let removed = timeline.delete_clip(pair.video, true).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(timeline.clip(pair.video).is_none());
        assert!(timeline.clip(pair.audio).is_none());
        timeline.check_invariants().unwrap();
    }

    #[test]
    #[ignore = "manual performance evidence; run with --ignored --nocapture"]
    fn fifty_thousand_clip_cache_timing_evidence() {
        use std::time::Instant;

        let clips = (1..=50_000)
            .map(|id| test_clip(id, (id as i64 - 1) * 2, 1))
            .collect();
        let (timeline, track_id) = cached_timeline_with_clips(clips);
        let started = Instant::now();
        let mut cache = TimelineCache::new();
        cache.rebuild_if_stale(&timeline);
        let records =
            cache
                .track(track_id)
                .unwrap()
                .draw_records(Tick(0), Tick(100_000), 0.01, 2.0);
        eprintln!(
            "50k cache rebuild + zoom-out banding: {:?}; {} draw records",
            started.elapsed(),
            records.len()
        );
    }

    #[test]
    #[ignore = "manual performance evidence; run in release with --ignored --nocapture"]
    fn fifty_thousand_clip_inverse_history_timing_evidence() {
        use std::time::{Duration, Instant};

        let clips = (1..=50_000)
            .map(|id| test_clip(id, (id as i64 - 1) * 3, 1))
            .collect();
        let (mut timeline, _) = cached_timeline_with_clips(clips);
        let snapshot_started = Instant::now();
        let before = timeline.snapshot();
        let snapshot_elapsed = snapshot_started.elapsed();
        timeline
            .move_clip_with_link(ClipId(25_000), Tick(1), false)
            .unwrap();
        let record_started = Instant::now();
        let mut history = UndoStack::default();
        assert!(history.record(&before, &timeline.snapshot()));
        let record_elapsed = record_started.elapsed();
        eprintln!(
            "50k inverse history capture: snapshot={snapshot_elapsed:?}, record={record_elapsed:?}"
        );
        assert!(
            record_elapsed < Duration::from_millis(2),
            "50k move history recording exceeded the 2 ms interaction budget: {record_elapsed:?}"
        );
    }

    fn color_effect(id: u32) -> VideoEffectNode {
        VideoEffectNode {
            id: VideoEffectId(id),
            enabled: true,
            kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
        }
    }

    fn brightness_contrast(operation: &EvaluatedVideoEffect) -> &EvaluatedBrightnessContrast {
        let EvaluatedVideoEffect::BrightnessContrast(effect) = operation else {
            panic!("expected brightness/contrast operation");
        };
        effect
    }

    fn vignette_effect(id: u32, enabled: bool) -> VideoEffectNode {
        VideoEffectNode {
            id: VideoEffectId(id),
            enabled,
            kind: VideoEffectKind::Vignette(VignetteEffect::default()),
        }
    }

    #[test]
    fn color_effect_evaluates_source_time_boundaries_linear_and_hold() {
        let mut effect = BrightnessContrastEffect::default();
        effect.brightness.keyframes = vec![
            ScalarKeyframe {
                source_tick: Tick(10),
                value: -0.5,
                interpolation: KeyframeInterpolation::Linear,
            },
            ScalarKeyframe {
                source_tick: Tick(20),
                value: 0.5,
                interpolation: KeyframeInterpolation::Hold,
            },
            ScalarKeyframe {
                source_tick: Tick(30),
                value: 0.8,
                interpolation: KeyframeInterpolation::Linear,
            },
        ];
        let node = VideoEffectNode {
            id: VideoEffectId(1),
            enabled: true,
            kind: VideoEffectKind::BrightnessContrast(effect),
        };
        let mut clip = test_clip(1, 0, 100);
        clip.video_effects = vec![node];
        assert_eq!(
            brightness_contrast(&clip.evaluate_video_effects(Tick(0)).active()[0]).brightness,
            -0.5
        );
        assert_eq!(
            brightness_contrast(&clip.evaluate_video_effects(Tick(15)).active()[0]).brightness,
            0.0
        );
        assert_eq!(
            brightness_contrast(&clip.evaluate_video_effects(Tick(25)).active()[0]).brightness,
            0.5
        );
        assert_eq!(
            brightness_contrast(&clip.evaluate_video_effects(Tick(40)).active()[0]).brightness,
            0.8
        );
        clip.video_effects[0].enabled = false;
        assert!(clip.evaluate_video_effects(Tick(15)).is_empty());
    }

    #[test]
    fn video_effect_stack_preserves_order_and_omits_disabled_nodes() {
        let mut clip = test_clip(1, 0, 100);
        let mut first = color_effect(10);
        let mut middle = color_effect(20);
        let mut last = color_effect(30);
        let VideoEffectKind::BrightnessContrast(effect) = &mut first.kind else {
            panic!()
        };
        effect.brightness.value = -0.25;
        let VideoEffectKind::BrightnessContrast(effect) = &mut middle.kind else {
            panic!()
        };
        effect.brightness.value = 0.5;
        let VideoEffectKind::BrightnessContrast(effect) = &mut last.kind else {
            panic!()
        };
        effect.brightness.value = 0.75;
        middle.enabled = false;
        clip.video_effects = vec![first, middle, last];

        let evaluated = clip.evaluate_video_effects(Tick(0));
        assert_eq!(evaluated.len(), 2);
        assert_eq!(
            evaluated
                .active()
                .iter()
                .map(|operation| brightness_contrast(operation).brightness)
                .collect::<Vec<_>>(),
            [-0.25, 0.75]
        );
    }

    #[test]
    fn vignette_defaults_clamp_animate_serialize_and_preserve_stack_history() {
        let default = VignetteEffect::default();
        assert_eq!(default.amount.value, 0.35);
        assert_eq!(default.midpoint.value, 0.45);
        assert_eq!(default.feather.value, 0.5);
        assert_eq!(default.center_x.value, 0.0);
        assert_eq!(default.center_y.value, 0.0);
        let legacy: VignetteEffect = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(legacy, default);

        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        timeline
            .set_clip_video_effects(
                clip,
                vec![
                    color_effect(1),
                    vignette_effect(2, true),
                    vignette_effect(3, false),
                ],
            )
            .unwrap();
        for (parameter, input, expected) in [
            (ColorParameter::VignetteAmount, 2.0, MAX_VIGNETTE_AMOUNT),
            (ColorParameter::VignetteMidpoint, 2.0, MAX_VIGNETTE_MIDPOINT),
            (ColorParameter::VignetteFeather, 0.0, MIN_VIGNETTE_FEATHER),
            (ColorParameter::VignetteCenterX, -2.0, MIN_VIGNETTE_CENTER),
            (ColorParameter::VignetteCenterY, 2.0, MAX_VIGNETTE_CENTER),
        ] {
            timeline
                .set_color_parameter(clip, VideoEffectId(2), parameter, input)
                .unwrap();
            assert_eq!(
                timeline
                    .clip(clip)
                    .and_then(|clip| color_scalar(clip, VideoEffectId(2), parameter))
                    .unwrap()
                    .value,
                expected
            );
        }
        assert!(matches!(
            timeline.set_color_parameter(
                clip,
                VideoEffectId(1),
                ColorParameter::VignetteAmount,
                0.5
            ),
            Err(TimelineError::InvalidVideoEffect)
        ));
        assert!(
            timeline
                .color_keyframe(
                    clip,
                    VideoEffectId(1),
                    ColorParameter::VignetteAmount,
                    Tick(0)
                )
                .is_none()
        );
        timeline
            .set_color_keyframe(
                clip,
                VideoEffectId(2),
                ColorParameter::VignetteAmount,
                Tick(10),
                0.0,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        timeline
            .set_color_keyframe(
                clip,
                VideoEffectId(2),
                ColorParameter::VignetteAmount,
                Tick(20),
                1.0,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        let evaluated = timeline
            .clip(clip)
            .unwrap()
            .evaluate_video_effects(Tick(15));
        assert_eq!(evaluated.len(), 2);
        assert!(matches!(
            evaluated.active()[0],
            EvaluatedVideoEffect::BrightnessContrast(_)
        ));
        assert!(matches!(
            evaluated.active()[1],
            EvaluatedVideoEffect::Vignette(EvaluatedVignette { amount, .. }) if amount == 0.5
        ));

        let snapshot = timeline.snapshot();
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(serialized.contains("\"type\":\"vignette\""));
        assert_eq!(
            Timeline::from_snapshot(snapshot.clone())
                .unwrap()
                .snapshot(),
            snapshot
        );
        let before = snapshot;
        timeline
            .set_color_parameter(
                clip,
                VideoEffectId(2),
                ColorParameter::VignetteCenterX,
                0.25,
            )
            .unwrap();
        let after = timeline.snapshot();
        let mut history = UndoStack::default();
        assert!(history.record(&before, &after));
        assert!(history.undo(&mut timeline));
        assert_eq!(timeline.snapshot(), before);
        assert!(history.redo(&mut timeline));
        assert_eq!(timeline.snapshot(), after);
    }

    #[test]
    fn animated_scalar_eases_use_the_documented_normalized_curves() {
        let evaluate = |interpolation, tick| {
            AnimatedScalar {
                value: -1.0,
                keyframes: vec![
                    ScalarKeyframe {
                        source_tick: Tick(10),
                        value: 0.0,
                        interpolation,
                    },
                    ScalarKeyframe {
                        source_tick: Tick(20),
                        value: 1.0,
                        interpolation: KeyframeInterpolation::Linear,
                    },
                ],
            }
            .evaluate(Tick(tick))
        };

        for interpolation in [
            KeyframeInterpolation::Smooth,
            KeyframeInterpolation::EaseIn,
            KeyframeInterpolation::EaseOut,
        ] {
            assert_eq!(evaluate(interpolation, 10), 0.0);
            assert_eq!(evaluate(interpolation, 20), 1.0);
        }
        for (interpolation, tick, expected) in [
            (KeyframeInterpolation::Smooth, 15, 0.5),
            (KeyframeInterpolation::EaseIn, 15, 0.25),
            (KeyframeInterpolation::EaseOut, 15, 0.75),
            (KeyframeInterpolation::Smooth, 12, 0.104),
            (KeyframeInterpolation::EaseIn, 12, 0.04),
            (KeyframeInterpolation::EaseOut, 12, 0.36),
        ] {
            assert!((evaluate(interpolation, tick) - expected).abs() < 0.000_001);
        }
    }

    #[test]
    fn keyframe_interpolation_serializes_new_variants_without_changing_existing_names() {
        for (interpolation, serialized) in [
            (KeyframeInterpolation::Linear, "\"Linear\""),
            (KeyframeInterpolation::Hold, "\"Hold\""),
            (KeyframeInterpolation::Smooth, "\"Smooth\""),
            (KeyframeInterpolation::EaseIn, "\"EaseIn\""),
            (KeyframeInterpolation::EaseOut, "\"EaseOut\""),
        ] {
            assert_eq!(serde_json::to_string(&interpolation).unwrap(), serialized);
            assert_eq!(
                serde_json::from_str::<KeyframeInterpolation>(serialized).unwrap(),
                interpolation
            );
        }
    }

    #[test]
    fn color_effect_snapshot_rejects_malformed_keys_and_nonfinite_values() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        timeline
            .set_clip_video_effects(clip, vec![color_effect(1)])
            .unwrap();

        let mut nonfinite = timeline.snapshot();
        let VideoEffectKind::BrightnessContrast(effect) =
            &mut nonfinite.tracks[0].clips[0].video_effects[0].kind
        else {
            panic!()
        };
        effect.brightness.value = f32::NAN;
        assert!(
            matches!(Timeline::from_snapshot(nonfinite), Err(TimelineSnapshotError::InvalidVideoEffect { clip: rejected }) if rejected == clip)
        );

        let mut unsorted = timeline.snapshot();
        let VideoEffectKind::BrightnessContrast(effect) =
            &mut unsorted.tracks[0].clips[0].video_effects[0].kind
        else {
            panic!()
        };
        effect.brightness.keyframes = vec![
            ScalarKeyframe {
                source_tick: Tick(2),
                value: 0.0,
                interpolation: KeyframeInterpolation::Linear,
            },
            ScalarKeyframe {
                source_tick: Tick(2),
                value: 0.1,
                interpolation: KeyframeInterpolation::Linear,
            },
        ];
        assert!(
            matches!(Timeline::from_snapshot(unsorted), Err(TimelineSnapshotError::InvalidVideoEffect { clip: rejected }) if rejected == clip)
        );
    }

    #[test]
    fn video_effect_stack_rejects_too_many_duplicate_and_zero_ids() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();

        let too_many = (1..=(MAX_VIDEO_EFFECTS_PER_CLIP as u32 + 1))
            .map(color_effect)
            .collect();
        assert!(matches!(
            timeline.set_clip_video_effects(clip, too_many),
            Err(TimelineError::InvalidVideoEffect)
        ));
        assert!(matches!(
            timeline.set_clip_video_effects(clip, vec![color_effect(1), color_effect(1)]),
            Err(TimelineError::InvalidVideoEffect)
        ));
        assert!(matches!(
            timeline.set_clip_video_effects(clip, vec![color_effect(0)]),
            Err(TimelineError::InvalidVideoEffect)
        ));
    }

    #[test]
    fn color_effect_setter_clamps_and_only_bumps_for_durable_changes() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let generation = timeline.generation();
        timeline
            .set_clip_video_effects(clip, vec![color_effect(1)])
            .unwrap();
        assert_eq!(timeline.generation(), generation + 1);
        timeline
            .set_clip_video_effects(clip, vec![color_effect(1)])
            .unwrap();
        assert_eq!(timeline.generation(), generation + 1);
        timeline
            .set_color_parameter(clip, VideoEffectId(1), ColorParameter::Brightness, 9.0)
            .unwrap();
        assert_eq!(
            brightness_contrast(
                &timeline
                    .clip(clip)
                    .unwrap()
                    .evaluate_video_effects(Tick(0))
                    .active()[0],
            )
            .brightness,
            MAX_BRIGHTNESS
        );
        assert!(
            timeline
                .set_color_parameter(clip, VideoEffectId(1), ColorParameter::Brightness, f32::NAN)
                .is_err()
        );
    }

    #[test]
    fn color_effect_restores_legacy_snapshots_and_round_trips() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(7))
            .unwrap();
        let mut legacy = serde_json::to_value(timeline.snapshot()).unwrap();
        for track in legacy["tracks"].as_array_mut().unwrap() {
            for clip in track["clips"].as_array_mut().unwrap() {
                clip.as_object_mut().unwrap().remove("video_effects");
            }
        }
        let restored = Timeline::from_snapshot(serde_json::from_value(legacy).unwrap()).unwrap();
        assert!(restored.clip(clip).unwrap().video_effects.is_empty());

        let mut timeline = restored;
        timeline
            .set_clip_video_effects(clip, vec![color_effect(1)])
            .unwrap();
        let snapshot = timeline.snapshot();
        assert_eq!(
            Timeline::from_snapshot(snapshot.clone())
                .unwrap()
                .snapshot(),
            snapshot
        );
    }

    #[test]
    fn basic_correction_parameters_default_restore_evaluate_and_clamp() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        timeline
            .set_clip_video_effects(clip, vec![color_effect(1)])
            .unwrap();

        let mut legacy = serde_json::to_value(timeline.snapshot()).unwrap();
        let effect = legacy["tracks"][0]["clips"][0]["video_effects"][0]
            .as_object_mut()
            .unwrap();
        for field in [
            "temperature",
            "tint",
            "saturation",
            "exposure",
            "highlights",
            "shadows",
            "whites",
            "blacks",
        ] {
            effect.remove(field);
        }
        let mut restored =
            Timeline::from_snapshot(serde_json::from_value(legacy).unwrap()).unwrap();
        let evaluated = restored
            .clip(clip)
            .unwrap()
            .evaluate_video_effects(Tick(0))
            .active()[0];
        let evaluated = brightness_contrast(&evaluated);
        assert_eq!(evaluated.temperature, 0.0);
        assert_eq!(evaluated.tint, 0.0);
        assert_eq!(evaluated.saturation, 1.0);
        assert_eq!(evaluated.exposure, 0.0);
        assert_eq!(evaluated.highlights, 0.0);
        assert_eq!(evaluated.shadows, 0.0);
        assert_eq!(evaluated.whites, 0.0);
        assert_eq!(evaluated.blacks, 0.0);

        for (parameter, value, expected) in [
            (ColorParameter::Temperature, 9.0, MAX_TEMPERATURE),
            (ColorParameter::Tint, -9.0, MIN_TINT),
            (ColorParameter::Saturation, 9.0, MAX_SATURATION),
            (ColorParameter::Exposure, -9.0, MIN_EXPOSURE),
            (ColorParameter::Highlights, 9.0, MAX_HIGHLIGHTS),
            (ColorParameter::Shadows, -9.0, MIN_SHADOWS),
            (ColorParameter::Whites, 9.0, MAX_WHITES),
            (ColorParameter::Blacks, -9.0, MIN_BLACKS),
        ] {
            restored
                .set_color_parameter(clip, VideoEffectId(1), parameter, value)
                .unwrap();
            let effect = restored
                .clip(clip)
                .unwrap()
                .evaluate_video_effects(Tick(0))
                .active()[0];
            let effect = brightness_contrast(&effect);
            let actual = match parameter {
                ColorParameter::Temperature => effect.temperature,
                ColorParameter::Tint => effect.tint,
                ColorParameter::Saturation => effect.saturation,
                ColorParameter::Exposure => effect.exposure,
                ColorParameter::Highlights => effect.highlights,
                ColorParameter::Shadows => effect.shadows,
                ColorParameter::Whites => effect.whites,
                ColorParameter::Blacks => effect.blacks,
                ColorParameter::Brightness
                | ColorParameter::Contrast
                | ColorParameter::VignetteAmount
                | ColorParameter::VignetteMidpoint
                | ColorParameter::VignetteFeather
                | ColorParameter::VignetteCenterX
                | ColorParameter::VignetteCenterY => unreachable!(),
            };
            assert_eq!(actual, expected);
        }
        restored
            .set_color_keyframe(
                clip,
                VideoEffectId(1),
                ColorParameter::Temperature,
                Tick(20),
                0.5,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        assert_eq!(
            brightness_contrast(
                &restored
                    .clip(clip)
                    .unwrap()
                    .evaluate_video_effects(Tick(20))
                    .active()[0],
            )
            .temperature,
            0.5
        );
        restored
            .set_color_keyframe(
                clip,
                VideoEffectId(1),
                ColorParameter::Whites,
                Tick(10),
                0.0,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        restored
            .set_color_keyframe(
                clip,
                VideoEffectId(1),
                ColorParameter::Whites,
                Tick(30),
                1.0,
                KeyframeInterpolation::Linear,
            )
            .unwrap();
        assert_eq!(
            brightness_contrast(
                &restored
                    .clip(clip)
                    .unwrap()
                    .evaluate_video_effects(Tick(20))
                    .active()[0],
            )
            .whites,
            0.5
        );
    }

    #[test]
    fn rgb_curves_restore_legacy_validate_and_compile_component_then_master_lut() {
        let identity = RgbCurves::default();
        assert!(normalize_rgb_curves(&identity));
        assert_eq!(compile_rgb_curve_lut(&identity), identity_curve_lut());

        let curves = RgbCurves {
            master: ColorCurve {
                points: vec![CurvePoint { x: 0.0, y: 0.0 }, CurvePoint { x: 1.0, y: 0.5 }],
            },
            red: ColorCurve {
                points: vec![CurvePoint { x: 0.0, y: 0.0 }, CurvePoint { x: 1.0, y: 1.0 }],
            },
            green: ColorCurve {
                points: vec![CurvePoint { x: 0.0, y: 1.0 }, CurvePoint { x: 1.0, y: 0.0 }],
            },
            blue: ColorCurve::default(),
        };
        assert!(normalize_rgb_curves(&curves));
        let lut = compile_rgb_curve_lut(&curves);
        assert_eq!(lut[COLOR_CURVE_LUT_SAMPLES - 1], [0.5, 0.0, 0.5, 0.0]);

        let invalid = ColorCurve {
            points: vec![
                CurvePoint { x: 0.0, y: 0.0 },
                CurvePoint { x: 0.001, y: 0.3 },
                CurvePoint { x: 1.0, y: 1.0 },
            ],
        };
        assert!(!normalize_color_curve(&invalid));
        let curved = ColorCurve {
            points: vec![
                CurvePoint { x: 0.0, y: 0.0 },
                CurvePoint { x: 0.4, y: 0.8 },
                CurvePoint { x: 1.0, y: 1.0 },
            ],
        };
        assert!(natural_curve_sample(&curved, 0.5) > 0.8);

        let tight = ColorCurve {
            points: vec![
                CurvePoint { x: 0.0, y: 0.0 },
                CurvePoint {
                    x: 1.0 / 255.0,
                    y: 1.0,
                },
                CurvePoint {
                    x: 2.0 / 255.0,
                    y: 0.0,
                },
                CurvePoint { x: 1.0, y: 1.0 },
            ],
        };
        assert!(normalize_color_curve(&tight));
        let tight_lut = compile_rgb_curve_lut(&RgbCurves {
            red: tight,
            ..Default::default()
        });
        assert_eq!(tight_lut[0][0], 0.0);
        assert_eq!(tight_lut[1][0], 1.0);
        assert_eq!(tight_lut[2][0], 0.0);

        let effect = BrightnessContrastEffect {
            curves,
            ..BrightnessContrastEffect::default()
        };
        let mut encoded = serde_json::to_value(effect).unwrap();
        encoded.as_object_mut().unwrap().remove("curves");
        let legacy: BrightnessContrastEffect = serde_json::from_value(encoded).unwrap();
        assert!(legacy.curves.is_identity());
    }

    #[test]
    fn razor_preserves_source_time_video_effect_keys_unchanged() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let clip = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(50))
            .unwrap();
        let mut node = color_effect(1);
        let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
            panic!()
        };
        effect.brightness.keyframes = vec![ScalarKeyframe {
            source_tick: Tick(75),
            value: 0.25,
            interpolation: KeyframeInterpolation::Linear,
        }];
        let mut second = color_effect(2);
        let VideoEffectKind::BrightnessContrast(effect) = &mut second.kind else {
            panic!()
        };
        effect.contrast.keyframes = vec![ScalarKeyframe {
            source_tick: Tick(75),
            value: 1.5,
            interpolation: KeyframeInterpolation::Linear,
        }];
        timeline
            .set_clip_video_effects(clip, vec![node, second])
            .unwrap();
        let split = timeline.razor(track, Tick(25)).unwrap().unwrap();
        for section in [split.left, split.right] {
            assert_eq!(
                timeline
                    .clip(section)
                    .unwrap()
                    .video_effects
                    .iter()
                    .map(|node| node.id)
                    .collect::<Vec<_>>(),
                [VideoEffectId(1), VideoEffectId(2)]
            );
            assert!(
                timeline
                    .color_keyframe(
                        section,
                        VideoEffectId(1),
                        ColorParameter::Brightness,
                        Tick(75)
                    )
                    .is_some()
            );
            assert!(
                timeline
                    .color_keyframe(
                        section,
                        VideoEffectId(2),
                        ColorParameter::Contrast,
                        Tick(75)
                    )
                    .is_some()
            );
        }
        assert_eq!(timeline.clip(split.right).unwrap().source_in, Tick(75));
    }

    fn adjacent_transition_timeline() -> (Timeline, TrackId, ClipId, ClipId) {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let left = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let right = timeline
            .insert_clip(track, MediaId(2), Tick(100), Tick(100), Tick(0))
            .unwrap();
        (timeline, track, left, right)
    }

    #[test]
    fn video_transitions_validate_and_center_timing() {
        let (mut timeline, track, left, right) = adjacent_transition_timeline();
        let structural = timeline.structural_generation();
        let id = timeline
            .add_video_transition(track, left, right, Tick(20), 4.0)
            .unwrap();
        assert_eq!(timeline.transition(id).unwrap().curve, MAX_FADE_CURVE);
        assert_eq!(
            timeline.transition(id).unwrap().kind,
            VideoTransitionKind::CrossDissolve
        );
        assert_eq!(timeline.transition_timing(id), Some((Tick(90), Tick(110))));
        assert_eq!(timeline.transition_progress(id, Tick(90)), Some(0.0));
        assert_eq!(timeline.transition_progress(id, Tick(100)), Some(0.5));
        assert_eq!(
            timeline.active_transition(Tick(109)).map(|item| item.id),
            Some(id)
        );
        assert_eq!(timeline.active_transition(Tick(110)), None);
        assert_eq!(timeline.structural_generation(), structural);
        assert!(matches!(
            timeline.add_video_transition(track, left, right, Tick(2), 0.0),
            Err(TimelineError::InvalidTransition)
        ));
        assert!(matches!(
            timeline.add_video_transition(track, right, left, Tick(2), 0.0),
            Err(TimelineError::InvalidTransition)
        ));
        assert!(matches!(
            timeline.add_video_transition(track, left, right, Tick(0), 0.0),
            Err(TimelineError::InvalidTransition)
        ));
    }

    #[test]
    fn video_transitions_prune_with_structural_clip_edits() {
        let (mut timeline, track, left, right) = adjacent_transition_timeline();
        let id = timeline
            .add_video_transition(track, left, right, Tick(20), 0.0)
            .unwrap();
        timeline.trim_start(right, Tick(1), false, false).unwrap();
        assert!(timeline.transition(id).is_none());

        assert!(
            timeline
                .add_video_transition(track, left, right, Tick(20), 0.0)
                .is_err(),
            "the trim made the cut non-adjacent"
        );
        timeline.move_clip(right, Tick(-1)).unwrap();
        let id = timeline
            .add_video_transition(track, left, right, Tick(20), 0.0)
            .unwrap();
        timeline.delete_clip(left, false).unwrap();
        assert!(timeline.transition(id).is_none());
    }

    #[test]
    fn video_transition_history_and_snapshot_compatibility() {
        let (mut timeline, track, left, right) = adjacent_transition_timeline();
        let before = timeline.snapshot();
        let id = timeline
            .add_video_transition_of_kind(
                track,
                left,
                right,
                Tick(20),
                0.25,
                VideoTransitionKind::DipToBlack,
            )
            .unwrap();
        let mut history = UndoStack::default();
        assert!(history.record_current(&before, &timeline));
        assert!(history.undo(&mut timeline));
        assert!(timeline.transition(id).is_none());
        assert!(history.redo(&mut timeline));
        assert_eq!(timeline.transition(id).unwrap().curve, 0.25);
        assert_eq!(
            timeline.transition(id).unwrap().kind,
            VideoTransitionKind::DipToBlack
        );

        let mut old = serde_json::to_value(timeline.snapshot()).unwrap();
        old["transitions"][0]
            .as_object_mut()
            .unwrap()
            .remove("kind");
        let restored =
            Timeline::from_snapshot(serde_json::from_value(old.clone()).unwrap()).unwrap();
        assert_eq!(
            restored.transition(id).unwrap().kind,
            VideoTransitionKind::CrossDissolve
        );

        old.as_object_mut().unwrap().remove("transitions");
        let restored = Timeline::from_snapshot(serde_json::from_value(old).unwrap()).unwrap();
        assert!(restored.transitions().is_empty());
    }

    #[test]
    fn all_video_transition_kinds_are_serializable_and_validate_at_a_cut() {
        let kinds = [
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
        for kind in kinds {
            let (mut timeline, track, left, right) = adjacent_transition_timeline();
            let id = timeline
                .add_video_transition_of_kind(track, left, right, Tick(20), 0.25, kind)
                .unwrap();
            assert_eq!(timeline.transition(id).unwrap().kind, kind);
            let snapshot = timeline.snapshot();
            let restored = Timeline::from_snapshot(snapshot).unwrap();
            assert_eq!(restored.transition(id).unwrap().kind, kind);
        }
    }

    #[test]
    fn adjacent_transition_windows_cannot_overlap_inside_the_middle_clip() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let left = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let middle = timeline
            .insert_clip(track, MediaId(2), Tick(100), Tick(40), Tick(0))
            .unwrap();
        let right = timeline
            .insert_clip(track, MediaId(3), Tick(140), Tick(100), Tick(0))
            .unwrap();
        timeline
            .add_video_transition(track, left, middle, Tick(60), 0.0)
            .unwrap();
        assert!(matches!(
            timeline.add_video_transition(track, middle, right, Tick(60), 0.0),
            Err(TimelineError::InvalidTransition)
        ));
        timeline
            .add_video_transition(track, middle, right, Tick(20), 0.0)
            .expect("30 ticks after the incoming cut plus 10 before the outgoing cut fit");
    }

    #[test]
    fn structural_shrink_prunes_a_newly_overlapping_neighbor_transition() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Video);
        let left = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let middle = timeline
            .insert_clip(track, MediaId(2), Tick(100), Tick(40), Tick(0))
            .unwrap();
        let right = timeline
            .insert_clip(track, MediaId(3), Tick(140), Tick(100), Tick(0))
            .unwrap();
        timeline
            .add_video_transition(track, left, middle, Tick(20), 0.0)
            .unwrap();
        timeline
            .add_video_transition(track, middle, right, Tick(20), 0.0)
            .unwrap();

        timeline
            .roll_edit(left, middle, Tick(25), false)
            .expect("the roll preserves both adjacent cuts while shrinking the middle clip");
        assert_eq!(timeline.clip(middle).unwrap().duration, Tick(15));
        assert_eq!(timeline.transitions().len(), 1);
        assert!(timeline.check_invariants().is_ok());
    }

    #[test]
    fn video_transition_snapshot_rejects_duplicate_and_exhausted_ids() {
        let (mut timeline, track, left, right) = adjacent_transition_timeline();
        let id = timeline
            .add_video_transition(track, left, right, Tick(20), 0.0)
            .unwrap();
        let mut duplicate = timeline.snapshot();
        let mut second = duplicate.transitions[0].clone();
        second.left_clip = right;
        second.right_clip = left;
        duplicate.transitions.push(second);
        assert!(
            matches!(Timeline::from_snapshot(duplicate), Err(TimelineSnapshotError::DuplicateTransitionId(rejected)) if rejected == id)
        );

        let mut exhausted = timeline.snapshot();
        exhausted.transitions[0].id = TransitionId(u32::MAX);
        assert!(matches!(
            Timeline::from_snapshot(exhausted),
            Err(TimelineSnapshotError::IdExhausted)
        ));
    }

    #[test]
    fn audio_transitions_validate_center_and_history_without_source_handles() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let left = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(99_000))
            .unwrap();
        let right = timeline
            .insert_clip(track, MediaId(2), Tick(100), Tick(100), Tick(0))
            .unwrap();
        let before = timeline.snapshot();
        let id = timeline
            .add_audio_transition(track, left, right, Tick(21))
            .unwrap();
        assert_eq!(
            timeline.audio_transition(id).unwrap().kind,
            AudioTransitionKind::EqualPowerCrossfade
        );
        assert_eq!(
            timeline.audio_transition_timing(id),
            Some((Tick(90), Tick(111)))
        );
        assert_eq!(
            timeline.audio_transition_progress(id, Tick(100)),
            Some(10.0 / 21.0)
        );
        assert_eq!(
            timeline
                .active_audio_transition(Tick(110))
                .map(|item| item.id),
            Some(id)
        );
        assert_eq!(timeline.active_audio_transition(Tick(111)), None);
        assert!(matches!(
            timeline.add_audio_transition(track, left, right, Tick(20)),
            Err(TimelineError::InvalidTransition)
        ));

        let mut history = UndoStack::default();
        assert!(history.record_current(&before, &timeline));
        assert!(history.undo(&mut timeline));
        assert!(timeline.audio_transition(id).is_none());
        assert!(history.redo(&mut timeline));
        timeline.replace_audio_transition(id, Tick(20)).unwrap();
        assert_eq!(timeline.audio_transition(id).unwrap().duration, Tick(20));
        assert_eq!(timeline.remove_audio_transition(id).unwrap().id, id);
    }

    #[test]
    fn audio_transition_snapshot_rejects_duplicate_ids_and_prunes_on_structural_edits() {
        let mut timeline = Timeline::new_default();
        let track = first_track(&timeline, TrackKind::Audio);
        let left = timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(100), Tick(0))
            .unwrap();
        let right = timeline
            .insert_clip(track, MediaId(2), Tick(100), Tick(100), Tick(0))
            .unwrap();
        let id = timeline
            .add_audio_transition(track, left, right, Tick(20))
            .unwrap();
        let mut duplicate = timeline.snapshot();
        duplicate
            .audio_transitions
            .push(duplicate.audio_transitions[0].clone());
        assert!(matches!(
            Timeline::from_snapshot(duplicate),
            Err(TimelineSnapshotError::DuplicateAudioTransitionId(rejected)) if rejected == id
        ));

        timeline.trim_start(right, Tick(1), false, false).unwrap();
        assert!(timeline.audio_transition(id).is_none());
        assert!(timeline.check_invariants().is_ok());
    }
}
